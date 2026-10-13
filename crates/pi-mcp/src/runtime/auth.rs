//! Auth of MCP connections. Connections never start a browser flow: they send the stored access
//! token and, after a 401, try the stored refresh token. Otherwise they fail with
//! `AuthorizationRequired`, and the user signs in (`sign_in_mcp_server`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use url::Url;

use super::signin::{McpOAuthSettings, callback_settings, create_provider, registered_redirect_urls};
use super::store::McpOAuthServerStore;
use crate::auth::{AuthProvider, UnauthorizedContext};
use crate::error::{McpError, OAuthError};
use crate::oauth::{
    McpOAuthStateStore, OAuthChallenge, OAuthFlowOptions, OAuthFlowResult, OAuthHttp, authorize_mcp,
    parse_www_authenticate,
};

/// Redirect URI for refreshes when none is stored. Refreshing never redirects the user.
pub(crate) const FALLBACK_REDIRECT_URL: &str = "http://127.0.0.1/callback";
/// Access tokens this close to expiry are refreshed before they are sent.
const REFRESH_SKEW_MS: i64 = 30_000;
/// Bounds each request to the authorization server, so an unresponsive one cannot hold the lock.
pub(crate) const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub type SettingsFn = Arc<dyn Fn() -> Result<McpOAuthSettings, String> + Send + Sync>;
pub type ChallengeFn = Arc<dyn Fn(OAuthChallenge) + Send + Sync>;
type SharedRefresh = Shared<BoxFuture<'static, Result<(), OAuthError>>>;

/// Sends the stored access token and refreshes it when it is about to expire or after a 401.
/// Requests in this process share one refresh, and other processes wait on the store's refresh lock,
/// since many servers rotate refresh tokens and a second refresh with the same token loses the grant.
pub struct McpAuthProvider {
    server_url: Url,
    store: Arc<McpOAuthServerStore>,
    /// Called only when a refresh is needed, so a secret that fails to resolve fails just the refresh.
    settings: SettingsFn,
    on_challenge: ChallengeFn,
    client: reqwest::Client,
    refreshing: Mutex<Option<SharedRefresh>>,
}

impl McpAuthProvider {
    pub fn new(
        server_url: Url,
        store: Arc<McpOAuthServerStore>,
        settings: SettingsFn,
        on_challenge: ChallengeFn,
        client: reqwest::Client,
    ) -> Self {
        Self { server_url, store, settings, on_challenge, client, refreshing: Mutex::new(None) }
    }

    fn current(&self) -> Option<SharedRefresh> {
        self.refreshing.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Resolves when no refresh is running, so shutdown does not drop rotated tokens before they are saved.
    pub async fn settled(&self) {
        if let Some(refresh) = self.current() {
            let _ = refresh.await;
        }
    }

    /// Replace `stale_token`, the access token that expired or was rejected.
    async fn refresh(
        &self,
        stale_token: Option<String>,
        client: reqwest::Client,
        challenge: Option<OAuthChallenge>,
    ) -> Result<(), OAuthError> {
        let refresh = {
            let mut refreshing = self.refreshing.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            refreshing
                .get_or_insert_with(|| {
                    let work = refresh_tokens(
                        self.server_url.clone(),
                        self.store.clone(),
                        self.settings.clone(),
                        stale_token,
                        client,
                        challenge,
                    );
                    // Not cancellable: a refresh the server answered may have rotated the refresh token.
                    let task = tokio::spawn(work);
                    async move { task.await.unwrap_or_else(|error| Err(OAuthError::other(error.to_string()))) }
                        .boxed()
                        .shared()
                })
                .clone()
        };
        let result = refresh.clone().await;
        let mut refreshing = self.refreshing.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if refreshing.as_ref().is_some_and(|current| current.ptr_eq(&refresh)) {
            *refreshing = None;
        }
        result
    }
}

async fn refresh_tokens(
    server_url: Url,
    store: Arc<McpOAuthServerStore>,
    settings: SettingsFn,
    stale_token: Option<String>,
    client: reqwest::Client,
    challenge: Option<OAuthChallenge>,
) -> Result<(), OAuthError> {
    store
        .clone()
        .with_refresh_lock(async move {
            let state = store.load().await?.unwrap_or_default();
            let tokens = state.tokens.as_ref();
            if tokens.map(|tokens| tokens.access_token.clone()) != stale_token {
                return Ok(());
            }
            if tokens.and_then(|tokens| tokens.refresh_token.as_ref()).is_none() {
                return Err(OAuthError::AuthorizationRequired);
            }
            let settings = settings().map_err(OAuthError::Other)?;
            let redirect_url = callback_settings(&settings)
                .fixed_redirect_url
                .or_else(|| registered_redirect_urls(state.client_information.as_ref()).into_iter().next())
                .unwrap_or_else(|| FALLBACK_REDIRECT_URL.into());
            let provider = create_provider(&server_url, store.clone(), &settings, redirect_url, Arc::new(|_| {}));
            let mut options =
                OAuthFlowOptions::new(server_url, OAuthHttp::new(client).with_timeout(OAUTH_REQUEST_TIMEOUT));
            options.resource_metadata_url =
                challenge.as_ref().and_then(|challenge| challenge.resource_metadata_url.clone());
            options.authorization_server_metadata_url = settings.auth_server_metadata_url.clone();
            options.scope = challenge.and_then(|challenge| challenge.scope);
            match authorize_mcp(&provider, &options).await? {
                OAuthFlowResult::Redirect => Err(OAuthError::AuthorizationRequired),
                OAuthFlowResult::Authorized => Ok(()),
            }
        })
        .await?
}

#[async_trait]
impl AuthProvider for McpAuthProvider {
    async fn token(&self) -> Result<Option<String>, McpError> {
        self.settled().await;
        let state = self.store.load().await?.unwrap_or_default();
        let token = state.tokens.as_ref().map(|tokens| tokens.access_token.clone());
        let expired = state.tokens_expire_at.is_some_and(|expires| expires - REFRESH_SKEW_MS <= pi_ai::now_ms());
        let refreshable = state.tokens.as_ref().is_some_and(|tokens| tokens.refresh_token.is_some());
        if !expired || !refreshable {
            return Ok(token);
        }
        // Failures fall through: the request goes out with the old token and a 401 decides.
        let _ = self.refresh(token, self.client.clone(), None).await;
        Ok(self.store.load().await?.and_then(|state| state.tokens).map(|tokens| tokens.access_token))
    }

    async fn on_unauthorized(&self, context: UnauthorizedContext) -> Result<bool, McpError> {
        let challenge = parse_www_authenticate(context.www_authenticate());
        (self.on_challenge)(challenge.clone());
        // A refresh keeps the granted scope, so more scope needs a new sign-in.
        if challenge.error.as_deref() == Some("insufficient_scope") {
            return Err(OAuthError::AuthorizationRequired.into());
        }
        self.refresh(context.token, context.client, Some(challenge)).await?;
        Ok(true)
    }
}

pub type ProviderTokenFn = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Sends the current token of a pi provider (`auth.provider`), read on every request so the
/// provider's own refreshes apply. MCP keeps no copy.
pub struct ProviderTokenAuth {
    pub provider: String,
    pub token: Option<ProviderTokenFn>,
}

#[async_trait]
impl AuthProvider for ProviderTokenAuth {
    async fn token(&self) -> Result<Option<String>, McpError> {
        Ok(match &self.token {
            Some(token) => token(self.provider.clone()).await,
            None => None,
        })
    }
}
