//! Anthropic OAuth flow (Claude Pro/Max).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::callback_server::{
    CallbackOrManual, CallbackServer, CallbackServerOptions, start_callback_server, wait_for_callback_or_manual_input,
};
use super::http::{Hooks, SendError, expires_at, parse_authorization_input, send};
use super::interact;
use super::pkce::{decode_constant, generate_pkce};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

const CLIENT_ID_ENCODED: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
// Preferred so the port can be forwarded into containers or over SSH. Anthropic accepts any
// loopback port, so login falls back to a free port when this one cannot be bound.
const CALLBACK_PORT: u16 = 53692;
const CALLBACK_PATH: &str = "/callback";
const COPY_CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const BROWSER_LOGIN_METHOD: &str = "browser";
const COPY_CODE_LOGIN_METHOD: &str = "copy_code";
const SCOPES: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

fn client_id() -> String {
    decode_constant(CLIENT_ID_ENCODED)
}

/// Anthropic Claude Pro/Max login with a browser callback or a copied code.
pub struct AnthropicOAuth {
    pub(crate) hooks: Hooks,
    pub(crate) callback_port: u16,
}

impl Default for AnthropicOAuth {
    fn default() -> Self {
        Self { hooks: Hooks::default(), callback_port: CALLBACK_PORT }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: f64,
}

fn authorize_url(challenge: &str, redirect_uri: &str, state: &str) -> String {
    let mut url = Url::parse(AUTHORIZE_URL).expect("static URL parses");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", &client_id())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

impl AnthropicOAuth {
    async fn post_json(&self, body: Value, cancel: &CancellationToken) -> Result<String, PostError> {
        let request = self.hooks.post(TOKEN_URL).header("Accept", "application/json").json(&body);
        let reply = send(request, cancel, self.hooks.timeout(REQUEST_TIMEOUT)).await.map_err(PostError::Send)?;
        if !reply.ok() {
            return Err(PostError::Status(format!(
                "HTTP request failed. status={}; url={TOKEN_URL}; body={}",
                reply.status, reply.body
            )));
        }
        Ok(reply.body)
    }

    async fn exchange_code(
        &self,
        code: &str,
        state: &str,
        verifier: &str,
        redirect_uri: &str,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let body = json!({
            "grant_type": "authorization_code",
            "client_id": client_id(),
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        });
        let text = self.post_json(body, cancel).await.map_err(|error| {
            error.wrap(|details| {
                format!(
                    "Token exchange request failed. url={TOKEN_URL}; redirect_uri={redirect_uri}; response_type=authorization_code; details={details}"
                )
            })
        })?;
        let token: TokenResponse = serde_json::from_str(&text).map_err(|error| {
            AuthError::other(format!(
                "Token exchange returned invalid JSON. url={TOKEN_URL}; body={text}; details=SyntaxError: {error}"
            ))
        })?;
        Ok(token_credential(token))
    }

    async fn start_server(
        &self,
        host: &str,
        port: u16,
        state: &str,
        cancel: &CancellationToken,
    ) -> AuthResult<CallbackServer<String>> {
        start_callback_server(CallbackServerOptions {
            provider_name: "Anthropic".into(),
            host: host.into(),
            port,
            path: CALLBACK_PATH.into(),
            redirect_host: Some("localhost".into()),
            state: Some(state.into()),
            complete: Arc::new(|code| async move { Ok(code) }.boxed()),
            cancel: Some(cancel.clone()),
            timeout: None,
        })
        .await
    }

    async fn login_browser(
        &self,
        interaction: &dyn AuthInteraction,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let pkce = generate_pkce();
        let host = self.hooks.callback_host();
        // Without a callback server, login continues with the pasted redirect URL.
        let callback = match self.start_server(&host, self.callback_port, &pkce.verifier, cancel).await {
            Ok(server) => Some(server),
            Err(_) => self.start_server(&host, 0, &pkce.verifier, cancel).await.ok(),
        };
        let fallback_redirect = format!("http://localhost:{}{CALLBACK_PATH}", self.callback_port);
        let redirect_uri = callback.as_ref().map_or(fallback_redirect, |server| server.redirect_uri().to_string());

        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url(&pkce.challenge, &redirect_uri, &pkce.verifier),
            instructions: Some(
                "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .into(),
            ),
        });
        let result = wait_for_callback_or_manual_input(
            interaction,
            callback.as_ref(),
            "Complete login in your browser, or paste the authorization code / redirect URL here:",
            &redirect_uri,
            cancel,
        )
        .await?;
        let (code, state) = match result {
            CallbackOrManual::Callback(code) => (Some(code), pkce.verifier.clone()),
            CallbackOrManual::Manual(input) => {
                let parsed = parse_authorization_input(&input);
                check_state(parsed.state.as_deref(), &pkce.verifier)?;
                (parsed.code, parsed.state.unwrap_or_else(|| pkce.verifier.clone()))
            }
        };
        let code =
            code.filter(|code| !code.is_empty()).ok_or_else(|| AuthError::other("Missing authorization code"))?;
        interaction.notify(AuthEvent::Progress { message: "Exchanging authorization code for tokens...".into() });
        // The callback server closes when `callback` drops, on every path.
        self.exchange_code(&code, &state, &pkce.verifier, &redirect_uri, cancel).await
    }

    async fn login_copy_code(
        &self,
        interaction: &dyn AuthInteraction,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let pkce = generate_pkce();
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url(&pkce.challenge, COPY_CODE_REDIRECT_URI, &pkce.verifier),
            instructions: Some(
                "Complete login in your browser, then copy the code Anthropic shows and paste it here.".into(),
            ),
        });
        let prompt = AuthPrompt::ManualCode {
            message: "Paste the code Anthropic shows after you sign in:".into(),
            placeholder: Some("code#state".into()),
        };
        let input = interact::prompt(interaction, prompt, cancel).await?;
        let parsed = parse_authorization_input(&input);
        check_state(parsed.state.as_deref(), &pkce.verifier)?;
        let code = parsed
            .code
            .filter(|code| !code.is_empty())
            .ok_or_else(|| AuthError::other("Missing authorization code"))?;
        interaction.notify(AuthEvent::Progress { message: "Exchanging authorization code for tokens...".into() });
        let state = parsed.state.unwrap_or_else(|| pkce.verifier.clone());
        self.exchange_code(&code, &state, &pkce.verifier, COPY_CODE_REDIRECT_URI, cancel).await
    }
}

fn check_state(state: Option<&str>, verifier: &str) -> AuthResult<()> {
    match state {
        Some(state) if !state.is_empty() && state != verifier => Err(AuthError::other("OAuth state mismatch")),
        _ => Ok(()),
    }
}

fn token_credential(token: TokenResponse) -> OAuthCredential {
    OAuthCredential {
        refresh: token.refresh_token,
        access: token.access_token,
        expires: expires_at(token.expires_in, EXPIRY_MARGIN_MS),
        extra: Map::new(),
    }
}

enum PostError {
    Send(SendError),
    Status(String),
}

impl PostError {
    /// Cancellation stays [`AuthError::Cancelled`]; other failures get upstream's diagnostic wrapper.
    fn wrap(self, message: impl FnOnce(String) -> String) -> AuthError {
        match self {
            Self::Send(SendError::Cancelled) => AuthError::Cancelled,
            Self::Send(error @ SendError::TimedOut) => AuthError::other(message(format!("TimeoutError: {error}"))),
            Self::Send(error) => AuthError::other(message(format!("TypeError: fetch failed; cause={error}"))),
            Self::Status(status) => AuthError::other(message(format!("Error: {status}"))),
        }
    }
}

#[async_trait]
impl OAuthAuth for AnthropicOAuth {
    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
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
        let options =
            [(BROWSER_LOGIN_METHOD, "Browser login (default)"), (COPY_CODE_LOGIN_METHOD, "Copy code login (headless)")];
        let method = interact::select(interaction, "Select Anthropic login method:", &options, &cancel).await?;
        match method.as_str() {
            COPY_CODE_LOGIN_METHOD => self.login_copy_code(interaction, &cancel).await,
            BROWSER_LOGIN_METHOD => self.login_browser(interaction, &cancel).await,
            other => Err(AuthError::other(format!("Unknown Anthropic login method: {other}"))),
        }
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        let body = json!({
            "grant_type": "refresh_token",
            "client_id": client_id(),
            "refresh_token": credential.refresh,
        });
        let text = self.post_json(body, &cancel).await.map_err(|error| {
            error.wrap(|details| format!("Anthropic token refresh request failed. url={TOKEN_URL}; details={details}"))
        })?;
        let token: TokenResponse = serde_json::from_str(&text).map_err(|error| {
            AuthError::other(format!(
                "Anthropic token refresh returned invalid JSON. url={TOKEN_URL}; body={text}; details=SyntaxError: {error}"
            ))
        })?;
        Ok(token_credential(token))
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
