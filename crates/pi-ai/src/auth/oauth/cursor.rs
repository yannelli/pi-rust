//! Cursor login as the Cursor CLI does it: the browser approves a PKCE challenge on
//! `cursor.com/loginDeepControl` while pi polls `api2.cursor.sh/auth/poll` with the verifier.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::http::{Hooks, SendError, decode_jwt_payload, now_ms, send, sleep};
use super::pkce::{generate_pkce, random_uuid};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

const LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
const POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
const REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
/// The OAuth client the Cursor IDE renews its login session with.
const CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const POLL_MAX_ATTEMPTS: u32 = 150;
const POLL_BASE_DELAY_MS: f64 = 1000.0;
const POLL_MAX_DELAY_MS: f64 = 10_000.0;
const POLL_BACKOFF: f64 = 1.2;
const MAX_CONSECUTIVE_ERRORS: u32 = 3;
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const DEFAULT_LIFETIME_MS: i64 = 3600 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Cursor login for Cursor Pro, Pro+, Ultra, and Business accounts.
#[derive(Default)]
pub struct CursorOAuth {
    pub(crate) hooks: Hooks,
}

/// The access token's `exp` less five minutes, else one hour from now.
pub(crate) fn token_expiry(token: &str) -> i64 {
    match decode_jwt_payload(token).and_then(|payload| payload.get("exp").and_then(Value::as_f64)) {
        Some(exp) => (exp * 1000.0) as i64 - REFRESH_SKEW_MS,
        None => now_ms() + DEFAULT_LIFETIME_MS,
    }
}

fn credential(access: String, refresh: String) -> OAuthCredential {
    let expires = token_expiry(&access);
    OAuthCredential { access, refresh, expires, extra: Map::new() }
}

/// The browser URL for `challenge`, and the `uuid` its approval is polled under.
pub(crate) fn login_url(challenge: &str, uuid: &str) -> String {
    let mut url = Url::parse(LOGIN_URL).expect("static URL parses");
    url.query_pairs_mut()
        .append_pair("challenge", challenge)
        .append_pair("uuid", uuid)
        .append_pair("mode", "login")
        .append_pair("redirectTarget", "cli");
    url.to_string()
}

impl CursorOAuth {
    async fn poll(&self, uuid: &str, verifier: &str, cancel: &CancellationToken) -> AuthResult<OAuthCredential> {
        let mut url = Url::parse(POLL_URL).expect("static URL parses");
        url.query_pairs_mut().append_pair("uuid", uuid).append_pair("verifier", verifier);
        let mut delay = POLL_BASE_DELAY_MS;
        let mut errors = 0;
        for _ in 0..POLL_MAX_ATTEMPTS {
            sleep(Duration::from_millis(delay as u64), cancel).await?;
            let reply = match send(self.hooks.get(url.as_str()), cancel, self.hooks.timeout(REQUEST_TIMEOUT)).await {
                Err(SendError::Cancelled) => return Err(AuthError::Cancelled),
                Err(_) => None,
                Ok(reply) if reply.status == 404 => {
                    errors = 0;
                    delay = (delay * POLL_BACKOFF).min(POLL_MAX_DELAY_MS);
                    continue;
                }
                Ok(reply) => reply.ok().then_some(reply),
            };
            let tokens = reply.and_then(|reply| reply.json().ok()).and_then(|body| {
                let access = body.get("accessToken").and_then(Value::as_str).filter(|token| !token.is_empty())?;
                let refresh = body.get("refreshToken").and_then(Value::as_str).unwrap_or_default();
                Some(credential(access.to_string(), refresh.to_string()))
            });
            match tokens {
                Some(credential) => return Ok(credential),
                None => errors += 1,
            }
            if errors >= MAX_CONSECUTIVE_ERRORS {
                return Err(AuthError::other("Too many consecutive errors during Cursor auth polling"));
            }
        }
        Err(AuthError::other("Cursor authentication polling timeout"))
    }
}

#[async_trait]
impl OAuthAuth for CursorOAuth {
    fn name(&self) -> &str {
        "Cursor (subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Cursor")
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let pkce = generate_pkce();
        let uuid = random_uuid();
        interaction.notify(AuthEvent::AuthUrl {
            url: login_url(&pkce.challenge, &uuid),
            instructions: Some("Sign in to Cursor in the browser. pi continues when the login completes.".into()),
        });
        interaction.notify(AuthEvent::Progress { message: "Waiting for browser authentication...".into() });
        self.poll(&uuid, &pkce.verifier, &cancel).await
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        let body =
            json!({ "grant_type": "refresh_token", "client_id": CLIENT_ID, "refresh_token": credential.refresh });
        let request = self.hooks.post(REFRESH_URL).json(&body);
        let reply = match send(request, &cancel, self.hooks.timeout(REQUEST_TIMEOUT)).await {
            Ok(reply) => reply,
            Err(SendError::Cancelled) => return Err(AuthError::Cancelled),
            Err(error) => return Err(AuthError::Http(error.to_string())),
        };
        if !reply.ok() {
            return Err(AuthError::other(format!("Cursor token refresh failed ({}): {}", reply.status, reply.body)));
        }
        let body = reply.json().map_err(|_| AuthError::other("Cursor token refresh returned invalid JSON"))?;
        // Cursor answers a session it will not renew with 200, an empty token, and `shouldLogout`.
        if body.get("shouldLogout").and_then(Value::as_bool) == Some(true) {
            return Err(AuthError::other("invalid_grant: Cursor ended this session; run /login cursor again"));
        }
        let access = body.get("access_token").and_then(Value::as_str).filter(|token| !token.is_empty());
        let access = access.ok_or_else(|| AuthError::other("Cursor token refresh returned no access token"))?;
        let rotated = body.get("refresh_token").and_then(Value::as_str).filter(|token| !token.is_empty());
        Ok(self::credential(access.to_string(), rotated.unwrap_or(&credential.refresh).to_string()))
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
