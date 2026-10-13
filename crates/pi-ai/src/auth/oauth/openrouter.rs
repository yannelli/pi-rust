//! OpenRouter OAuth PKCE flow. The authorization code is exchanged for a permanent,
//! user-controlled API key instead of an expiring token pair.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use reqwest::Url;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::callback_server::{
    CallbackOrManual, CallbackServerOptions, start_callback_server, wait_for_callback_or_manual_input,
};
use super::http::{Hooks, SendError, search_param, send, url_param};
use super::pkce::{generate_pkce, random_uuid};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// `Number.MAX_SAFE_INTEGER`: the key never expires.
const NEVER_EXPIRES: i64 = 9_007_199_254_740_991;

/// OpenRouter sign-in that mints an API key.
#[derive(Default)]
pub struct OpenRouterOAuth {
    pub(crate) hooks: Hooks,
}

fn parse_authorization_input(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = Url::parse(value) {
        return url_param(&url, "code");
    }
    if value.contains("code=") {
        return search_param(value, "code");
    }
    Some(value.to_string())
}

fn error_detail(body: &Map<String, Value>) -> Option<String> {
    for key in ["error_description", "message", "error"] {
        if let Some(text) = body.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    body.get("error")?.as_object()?.get("message")?.as_str().map(str::to_string)
}

async fn exchange_code(
    hooks: &Hooks,
    code: &str,
    verifier: &str,
    cancel: &CancellationToken,
) -> AuthResult<OAuthCredential> {
    if cancel.is_cancelled() {
        return Err(AuthError::Cancelled);
    }
    let body = json!({ "code": code, "code_verifier": verifier, "code_challenge_method": "S256" });
    let request = hooks.post(TOKEN_URL).header("accept", "application/json").json(&body);
    let reply = match send(request, cancel, hooks.timeout(TOKEN_EXCHANGE_TIMEOUT)).await {
        Ok(reply) => reply,
        Err(SendError::Cancelled) => return Err(AuthError::Cancelled),
        Err(SendError::TimedOut) => return Err(AuthError::other("OpenRouter OAuth token exchange timed out")),
        Err(error) => return Err(AuthError::Http(error.to_string())),
    };
    let body = match reply.json() {
        Ok(Value::Object(body)) => body,
        Ok(_) => Map::new(),
        Err(_) if reply.ok() => return Err(AuthError::other("OpenRouter OAuth returned invalid JSON")),
        Err(_) => Map::new(),
    };
    if !reply.ok() {
        let detail = error_detail(&body).filter(|detail| !detail.is_empty());
        let detail = detail.map(|detail| format!(": {detail}")).unwrap_or_default();
        return Err(AuthError::other(format!("OpenRouter OAuth key exchange failed (HTTP {}){detail}", reply.status)));
    }
    let key = body.get("key").and_then(Value::as_str).filter(|key| !key.is_empty());
    let key = key.ok_or_else(|| AuthError::other("OpenRouter OAuth response carries no \"key\""))?;
    Ok(OAuthCredential { access: key.into(), refresh: String::new(), expires: NEVER_EXPIRES, extra: Map::new() })
}

impl OpenRouterOAuth {
    async fn login_openrouter(
        &self,
        interaction: &dyn AuthInteraction,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let pkce = generate_pkce();
        let (hooks, verifier, exchange_cancel) = (self.hooks.clone(), pkce.verifier.clone(), cancel.clone());
        // OpenRouter sends no `state`; the random path keeps stray requests from completing sign-in.
        let callback = start_callback_server(CallbackServerOptions {
            provider_name: "OpenRouter".into(),
            host: self.hooks.callback_host(),
            port: 0,
            path: format!("/oauth/callback/{}", random_uuid()),
            redirect_host: None,
            state: None,
            complete: Arc::new(move |code| {
                let (hooks, verifier, cancel) = (hooks.clone(), verifier.clone(), exchange_cancel.clone());
                async move { exchange_code(&hooks, &code, &verifier, &cancel).await }.boxed()
            }),
            cancel: Some(cancel.clone()),
            timeout: Some(LOGIN_TIMEOUT),
        })
        .await?;

        let mut url = Url::parse(AUTHORIZE_URL).expect("static URL parses");
        url.query_pairs_mut()
            .append_pair("callback_url", callback.redirect_uri())
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256");
        interaction.notify(AuthEvent::Progress {
            message: format!("Listening for OpenRouter OAuth callback on {}", callback.redirect_uri()),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: url.to_string(),
            instructions: Some(
                "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .into(),
            ),
        });

        let result = wait_for_callback_or_manual_input(
            interaction,
            Some(&callback),
            "Complete sign-in in your browser, or paste the authorization code / redirect URL here:",
            callback.redirect_uri(),
            cancel,
        )
        .await?;
        let input = match result {
            CallbackOrManual::Callback(credential) => return Ok(credential),
            CallbackOrManual::Manual(input) => input,
        };
        let code = parse_authorization_input(&input).filter(|code| !code.is_empty());
        let code = code.ok_or_else(|| AuthError::other("Missing authorization code"))?;
        interaction.notify(AuthEvent::Progress { message: "Exchanging authorization code for an API key...".into() });
        exchange_code(&self.hooks, &code, &pkce.verifier, cancel).await
    }
}

#[async_trait]
impl OAuthAuth for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with OpenRouter")
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        self.login_openrouter(interaction, &cancel).await
    }

    /// The key is permanent, so refresh returns the stored credential unchanged.
    async fn refresh(&self, credential: &OAuthCredential, _cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        Ok(credential.clone())
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
