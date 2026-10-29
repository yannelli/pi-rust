//! OpenAI Responses API token sharing through Sign in with ChatGPT. This public-client flow
//! uses no client secret and sends the user access token directly to api.openai.com.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::FutureExt;
use reqwest::Url;
use serde_json::{Map, Value};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::http::{Hooks, expires_at, url_param};
use super::loopback::{LoopbackRequest, LoopbackResponse, LoopbackServer};
use super::page::{oauth_error_html, oauth_success_html};
use super::pkce::{generate_pkce, random_base64url};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

// Every login registers a new client with this id; OpenAI returns the issued id in the callback.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AGENT_NAME_HINT: &str = "Pi";
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
// Refresh this long before the real expiry so a request never starts with a token about to expire.
const EXPIRY_MARGIN_MS: i64 = 3 * 60 * 1000;

/// Sign in with ChatGPT for the `openai` provider.
pub struct OpenAIChatGPTOAuth {
    pub(crate) hooks: Hooks,
    pub(crate) callback_port: u16,
}

impl Default for OpenAIChatGPTOAuth {
    fn default() -> Self {
        Self { hooks: Hooks::default(), callback_port: CALLBACK_PORT }
    }
}

#[derive(Debug)]
struct Authorization {
    code: String,
    client_id: String,
}

fn authorization_from_callback(url: &Url, expected_state: &str) -> AuthResult<Authorization> {
    let code = url_param(url, "code").filter(|code| !code.is_empty());
    let code = code.ok_or_else(|| AuthError::other("Missing authorization code"))?;
    let state = url_param(url, "state").filter(|state| !state.is_empty());
    let state = state.ok_or_else(|| AuthError::other("Missing OAuth state"))?;
    if state != expected_state {
        return Err(AuthError::other("OAuth state mismatch"));
    }
    let client_id = url_param(url, "client_id").map(|id| id.trim().to_string()).filter(|id| !id.is_empty());
    let client_id = client_id
        .ok_or_else(|| AuthError::other("OpenAI OAuth registration callback did not contain an issued client ID"))?;
    Ok(Authorization { code, client_id })
}

fn authorization_from_manual_input(input: &str, expected_state: &str, redirect_uri: &str) -> AuthResult<Authorization> {
    let url = Url::parse(input.trim()).map_err(|_| AuthError::other("Paste the full callback URL from the browser"))?;
    let expected = Url::parse(redirect_uri).expect("redirect URI parses");
    if url.origin() != expected.origin() || url.path() != expected.path() {
        return Err(AuthError::other(format!("The pasted callback URL must start with {redirect_uri}")));
    }
    if let Some(error) = url_param(&url, "error").filter(|error| !error.is_empty()) {
        return Err(AuthError::other(format!("ChatGPT authorization failed: {error}")));
    }
    authorization_from_callback(&url, expected_state)
}

type ResultSender = Arc<Mutex<Option<oneshot::Sender<AuthResult<Authorization>>>>>;

fn handle_callback(request: &LoopbackRequest, expected_state: &str, sender: &ResultSender) -> LoopbackResponse {
    let page =
        |status, body| LoopbackResponse { status, headers: vec![("content-type", "text/html; charset=utf-8")], body };
    let resolve = |result| {
        if let Some(sender) = sender.lock().expect("callback sender lock").take() {
            let _ = sender.send(result);
        }
    };
    if request.url.path() != CALLBACK_PATH {
        return page(404, oauth_error_html("Callback route not found.", None));
    }
    if let Some(error) = url_param(&request.url, "error").filter(|error| !error.is_empty()) {
        resolve(Err(AuthError::other(format!("ChatGPT authorization failed: {error}"))));
        return page(400, oauth_error_html("ChatGPT was not connected.", Some(&format!("Error: {error}"))));
    }
    match authorization_from_callback(&request.url, expected_state) {
        Ok(authorization) => {
            resolve(Ok(authorization));
            page(200, oauth_success_html("ChatGPT authentication completed. You can close this window."))
        }
        Err(error) => page(400, oauth_error_html(&error.to_string(), None)),
    }
}

fn require_token_string(token: &Map<String, Value>, field: &str) -> AuthResult<String> {
    match token.get(field).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_string()),
        _ => Err(AuthError::other(format!("OpenAI OAuth token response has invalid {field}"))),
    }
}

fn credential_from_token(token: &Map<String, Value>, client_id: &str) -> AuthResult<OAuthCredential> {
    let access = require_token_string(token, "access_token")?;
    let refresh = require_token_string(token, "refresh_token")?;
    let scope = require_token_string(token, "scope")?;
    let expires_in = token.get("expires_in").and_then(Value::as_f64).filter(|value| value.is_finite() && *value > 0.0);
    let expires_in =
        expires_in.ok_or_else(|| AuthError::other("OpenAI OAuth token response has invalid expires_in"))?;
    let scopes: Vec<Value> = scope.split_whitespace().map(|scope| Value::String(scope.into())).collect();
    if !scopes.iter().any(|scope| scope == DIRECT_TOKEN_SCOPE) {
        return Err(AuthError::other(format!("OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}")));
    }
    let mut extra = Map::new();
    extra.insert("clientId".into(), Value::String(client_id.into()));
    extra.insert("scopes".into(), Value::Array(scopes));
    Ok(OAuthCredential { access, refresh, expires: expires_at(expires_in, EXPIRY_MARGIN_MS), extra })
}

/// OpenAI identifies each installation ("agent host") by a stable URI such as `urn:uuid:<uuid>`.
fn agent_host_id(device_id: Option<String>) -> AuthResult<String> {
    match device_id {
        Some(id) if is_uuid(&id) => Ok(format!("urn:uuid:{}", id.to_lowercase())),
        _ => Err(AuthError::other("Sign in with ChatGPT requires a device ID (UUID) for this installation")),
    }
}

fn is_uuid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];
    groups.len() == 5
        && groups
            .iter()
            .zip(lengths)
            .all(|(group, length)| group.len() == length && group.chars().all(|char| char.is_ascii_hexdigit()))
}

impl OpenAIChatGPTOAuth {
    async fn request_token(&self, form: &[(&str, &str)], cancel: &CancellationToken) -> AuthResult<Map<String, Value>> {
        let request = self.hooks.post(TOKEN_URL).header("accept", "application/json").form(form);
        let reply = super::http::send(request, cancel, None).await?;
        if !reply.ok() {
            let text = if reply.body.is_empty() { reply.status_text().to_string() } else { reply.body.clone() };
            return Err(AuthError::other(format!("OpenAI OAuth token request failed ({}): {text}", reply.status)));
        }
        match reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))? {
            Value::Object(object) => Ok(object),
            _ => Err(AuthError::other("OpenAI OAuth token response must be an object")),
        }
    }

    async fn exchange_code(
        &self,
        authorization: &Authorization,
        verifier: &str,
        redirect_uri: &str,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", authorization.client_id.as_str()),
            ("code", authorization.code.as_str()),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
            ("resource", RESOURCE),
        ];
        let token = self.request_token(&form, cancel).await?;
        // Pi does not read the ID token; its presence is part of the token-response contract.
        if token.get("id_token").and_then(Value::as_str).is_none_or(|id| id.trim().is_empty()) {
            return Err(AuthError::other("OpenAI OAuth token response did not contain an ID token"));
        }
        credential_from_token(&token, &authorization.client_id)
    }

    async fn start_server(
        &self,
        state: &str,
    ) -> AuthResult<(LoopbackServer, oneshot::Receiver<AuthResult<Authorization>>)> {
        let (sender, receiver) = oneshot::channel();
        let sender: ResultSender = Arc::new(Mutex::new(Some(sender)));
        let state = state.to_string();
        let handler = Arc::new(move |request: LoopbackRequest| {
            let response = handle_callback(&request, &state, &sender);
            async move { response }.boxed()
        });
        // Without this server the callback reaches whatever else holds the port (another pending
        // login or the Codex CLI), which rejects it as a state mismatch. Fail clearly instead.
        let server = LoopbackServer::bind(&self.hooks.callback_host(), self.callback_port, handler).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                AuthError::other(format!(
                    "Port {} is in use, probably by an unfinished login in another pi session or by the Codex CLI. Cancel that login and try again.",
                    self.callback_port
                ))
            } else {
                AuthError::other(error.to_string())
            }
        })?;
        Ok((server, receiver))
    }

    async fn login_chatgpt(
        &self,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let host_id = agent_host_id(options.device_id.as_ref().map(|device_id| device_id()))?;
        let pkce = generate_pkce();
        let state = random_base64url();
        let nonce = random_base64url();
        let (server, callback) = self.start_server(&state).await?;
        let redirect_uri = format!("http://127.0.0.1:{}{CALLBACK_PATH}", server.port);

        let mut url = Url::parse(AUTHORIZE_URL).expect("static URL parses");
        url.query_pairs_mut()
            .append_pair("client_id", DYNAMIC_CLIENT_ID)
            .append_pair("agent_name_hint", options.agent_name.as_deref().unwrap_or(AGENT_NAME_HINT))
            .append_pair("ext_agent_host_id", &host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("resource", RESOURCE)
            .append_pair("scope", SCOPE)
            .append_pair("state", &state)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("nonce", &nonce);
        interaction.notify(AuthEvent::AuthUrl {
            url: url.to_string(),
            instructions: Some(
                "Complete sign-in in your browser. If the callback does not complete, paste the final redirect URL here."
                    .into(),
            ),
        });

        let manual_cancel = cancel.child_token();
        let dismiss = manual_cancel.clone().drop_guard();
        let prompt = AuthPrompt::ManualCode {
            message: "Complete login in your browser, or paste the final redirect URL here:".into(),
            placeholder: Some(redirect_uri.clone()),
        };
        let manual = async {
            let input = interaction.prompt(prompt, manual_cancel.clone()).await?;
            authorization_from_manual_input(&input, &state, &redirect_uri)
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AuthError::Cancelled),
            received = callback => received.unwrap_or_else(|_| Err(AuthError::other("OAuth callback server closed"))),
            manual = manual => manual,
        };
        // Closing also drops idle browser connections, so a later login's callback cannot land here.
        drop(dismiss);
        server.close();
        let outcome = match result {
            Ok(authorization) => {
                interaction
                    .notify(AuthEvent::Progress { message: "Exchanging authorization code for tokens...".into() });
                self.exchange_code(&authorization, &pkce.verifier, &redirect_uri, cancel).await
            }
            Err(error) => Err(error),
        };
        outcome.map_err(|error| if cancel.is_cancelled() { AuthError::Cancelled } else { error })
    }
}

#[async_trait]
impl OAuthAuth for OpenAIChatGPTOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with ChatGPT")
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        self.login_chatgpt(interaction, options, &cancel).await
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        let client_id = credential.extra_str("clientId").filter(|id| !id.trim().is_empty());
        let client_id = client_id.ok_or_else(|| {
            AuthError::other("Stored OpenAI OAuth credential does not contain an issued client ID; reconnect ChatGPT")
        })?;
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", credential.refresh.as_str()),
            ("resource", RESOURCE),
        ];
        let token = self.request_token(&form, &cancel).await?;
        credential_from_token(&token, client_id)
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
