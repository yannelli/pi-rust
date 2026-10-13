//! A stateful OAuth client provider for one MCP server URL, and the transport auth adapter.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde::{Deserialize, Deserializer, Serialize};
use url::Url;

use super::discovery::parse_www_authenticate;
use super::flow::{
    ClientMetadataDocument, InvalidateKind, OAuthClientProvider, OAuthFlowOptions, OAuthFlowResult, authorize_mcp,
};
use super::http::OAuthHttp;
use super::pkce::random_state;
use super::requests::step_up_scope;
use super::types::{
    AuthorizationServerMetadata, OAuthClientInformation, OAuthClientMetadata, OAuthDiscoveryState, OAuthTokens,
};
use crate::auth::{AuthProvider, UnauthorizedContext};
use crate::error::{McpError, OAuthError};

fn number_as_i64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<f64>::deserialize(deserializer)?.map(|value| value as i64))
}

/// OAuth state of one server, as `mcp-auth.json` stores it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthState {
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_information: Option<OAuthClientInformation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<OAuthTokens>,
    /// When the access token expires, in milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "number_as_i64")]
    pub tokens_expire_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<OAuthDiscoveryState>,
}

#[async_trait]
pub trait McpOAuthStateStore: Send + Sync {
    async fn load(&self) -> Result<Option<McpOAuthState>, OAuthError>;
    async fn save(&self, state: McpOAuthState) -> Result<(), OAuthError>;
}

#[derive(Default)]
pub struct MemoryOAuthStateStore {
    value: Mutex<Option<McpOAuthState>>,
}

#[async_trait]
impl McpOAuthStateStore for MemoryOAuthStateStore {
    async fn load(&self) -> Result<Option<McpOAuthState>, OAuthError> {
        Ok(self.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone())
    }

    async fn save(&self, state: McpOAuthState) -> Result<(), OAuthError> {
        *self.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state);
        Ok(())
    }
}

pub type ClientMetadataDocumentFn = Arc<
    dyn Fn(Option<&AuthorizationServerMetadata>) -> Result<Option<ClientMetadataDocument>, OAuthError> + Send + Sync,
>;

pub struct McpOAuthProviderOptions {
    pub server_url: Url,
    pub redirect_url: String,
    /// Defaults fill `redirect_uris`, `grant_types`, `response_types`, and `token_endpoint_auth_method`.
    pub client_metadata: OAuthClientMetadata,
    pub client_metadata_document: Option<ClientMetadataDocumentFn>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub store: Arc<dyn McpOAuthStateStore>,
    pub on_redirect: Arc<dyn Fn(&Url) + Send + Sync>,
}

/// Provider bound to one exact server URL: stored state of another URL is ignored, so credentials
/// never leak across servers.
pub struct McpOAuthProvider {
    server_url: String,
    redirect_url: String,
    client_metadata: OAuthClientMetadata,
    client_metadata_document: Option<ClientMetadataDocumentFn>,
    configured_client: Option<OAuthClientInformation>,
    store: Arc<dyn McpOAuthStateStore>,
    on_redirect: Arc<dyn Fn(&Url) + Send + Sync>,
    writes: tokio::sync::Mutex<()>,
}

impl McpOAuthProvider {
    pub fn new(options: McpOAuthProviderOptions) -> Self {
        let mut metadata = options.client_metadata;
        if metadata.redirect_uris.is_empty() {
            metadata.redirect_uris = vec![options.redirect_url.clone()];
        }
        metadata.grant_types.get_or_insert_with(|| vec!["authorization_code".into(), "refresh_token".into()]);
        metadata.response_types.get_or_insert_with(|| vec!["code".into()]);
        let auth_method = if options.client_secret.is_some() { "client_secret_post" } else { "none" };
        metadata.token_endpoint_auth_method.get_or_insert_with(|| auth_method.into());
        let configured_client = options.client_id.filter(|id| !id.is_empty()).map(|client_id| OAuthClientInformation {
            client_id,
            client_secret: options.client_secret.filter(|secret| !secret.is_empty()),
            ..OAuthClientInformation::default()
        });
        Self {
            server_url: options.server_url.to_string(),
            redirect_url: options.redirect_url,
            client_metadata: metadata,
            client_metadata_document: options.client_metadata_document,
            configured_client,
            store: options.store,
            on_redirect: options.on_redirect,
            writes: tokio::sync::Mutex::new(()),
        }
    }

    fn own(&self, state: Option<McpOAuthState>) -> McpOAuthState {
        state
            .filter(|state| state.server_url == self.server_url)
            .unwrap_or_else(|| McpOAuthState { server_url: self.server_url.clone(), ..McpOAuthState::default() })
    }

    pub async fn load(&self) -> Result<McpOAuthState, OAuthError> {
        let _writes = self.writes.lock().await;
        Ok(self.own(self.store.load().await?))
    }

    async fn update(&self, change: impl FnOnce(&mut McpOAuthState)) -> Result<(), OAuthError> {
        let _writes = self.writes.lock().await;
        let mut state = self.own(self.store.load().await?);
        change(&mut state);
        self.store.save(state).await
    }

    /// The `state` parameter, created once per sign-in.
    pub async fn oauth_state(&self) -> Result<String, OAuthError> {
        if let Some(existing) = self.load().await?.oauth_state.filter(|state| !state.is_empty()) {
            return Ok(existing);
        }
        let state = random_state();
        let value = state.clone();
        self.update(|current| current.oauth_state = Some(value)).await?;
        Ok(state)
    }
}

#[async_trait]
impl OAuthClientProvider for McpOAuthProvider {
    fn redirect_url(&self) -> String {
        self.redirect_url.clone()
    }

    fn client_metadata(&self) -> OAuthClientMetadata {
        self.client_metadata.clone()
    }

    fn client_metadata_document(
        &self,
        metadata: Option<&AuthorizationServerMetadata>,
    ) -> Result<Option<ClientMetadataDocument>, OAuthError> {
        match &self.client_metadata_document {
            Some(document) => document(metadata),
            None => Ok(None),
        }
    }

    async fn state(&self) -> Result<Option<String>, OAuthError> {
        self.oauth_state().await.map(Some)
    }

    async fn client_information(&self) -> Result<Option<OAuthClientInformation>, OAuthError> {
        if let Some(client) = &self.configured_client {
            return Ok(Some(client.clone()));
        }
        Ok(self.load().await?.client_information)
    }

    async fn save_client_information(&self, information: OAuthClientInformation) -> Result<(), OAuthError> {
        if self.configured_client.is_some() {
            return Ok(());
        }
        self.update(|state| state.client_information = Some(information)).await
    }

    async fn tokens(&self) -> Result<Option<OAuthTokens>, OAuthError> {
        Ok(self.load().await?.tokens)
    }

    async fn save_tokens(&self, tokens: OAuthTokens) -> Result<(), OAuthError> {
        let expires_at = tokens.expires_in.map(|seconds| pi_ai::now_ms() + (seconds * 1000.0) as i64);
        self.update(|state| {
            state.tokens = Some(tokens);
            state.tokens_expire_at = expires_at;
        })
        .await
    }

    async fn redirect_to_authorization(&self, url: Url) -> Result<(), OAuthError> {
        (self.on_redirect)(&url);
        Ok(())
    }

    async fn save_code_verifier(&self, verifier: String) -> Result<(), OAuthError> {
        self.update(|state| state.code_verifier = Some(verifier)).await
    }

    async fn code_verifier(&self) -> Result<String, OAuthError> {
        let verifier = self.load().await?.code_verifier.filter(|verifier| !verifier.is_empty());
        verifier.ok_or_else(|| OAuthError::other("No OAuth PKCE code verifier is stored"))
    }

    async fn invalidate_credentials(&self, kind: InvalidateKind) -> Result<(), OAuthError> {
        use InvalidateKind::*;
        self.update(|state| {
            if matches!(kind, All | Client) {
                state.client_information = None;
            }
            if matches!(kind, All | Tokens) {
                state.tokens = None;
                state.tokens_expire_at = None;
            }
            if matches!(kind, All | Verifier) {
                state.code_verifier = None;
            }
            if matches!(kind, All | Discovery) {
                state.discovery = None;
            }
            if kind == All {
                state.oauth_state = None;
            }
        })
        .await
    }

    async fn save_discovery_state(&self, discovery: OAuthDiscoveryState) -> Result<(), OAuthError> {
        self.update(|state| state.discovery = Some(discovery)).await
    }

    async fn discovery_state(&self) -> Result<Option<OAuthDiscoveryState>, OAuthError> {
        Ok(self.load().await?.discovery)
    }
}

type SharedFlow = Shared<BoxFuture<'static, Result<(), OAuthError>>>;

/// Transport auth for an OAuth client provider: refreshes after a 401, or fails with
/// `AuthorizationRequired`. Concurrent 401s share one refresh, since rotating refresh tokens
/// make a second refresh with the old token fail and discard the new grant.
pub struct OAuthProviderAuth {
    provider: Arc<dyn OAuthClientProvider>,
    in_flight: Mutex<Option<SharedFlow>>,
}

impl OAuthProviderAuth {
    pub fn new(provider: Arc<dyn OAuthClientProvider>) -> Self {
        Self { provider, in_flight: Mutex::new(None) }
    }

    fn in_flight(&self) -> std::sync::MutexGuard<'_, Option<SharedFlow>> {
        self.in_flight.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl AuthProvider for OAuthProviderAuth {
    async fn token(&self) -> Result<Option<String>, McpError> {
        Ok(self.provider.tokens().await?.map(|tokens| tokens.access_token))
    }

    async fn on_unauthorized(&self, context: UnauthorizedContext) -> Result<bool, McpError> {
        let challenge = parse_www_authenticate(context.www_authenticate());
        let insufficient_scope = challenge.error.as_deref() == Some("insufficient_scope");
        // A request whose token another request already replaced is just retried.
        if !insufficient_scope && self.in_flight().is_none() && context.token.is_some() {
            let current = self.provider.tokens().await?.map(|tokens| tokens.access_token);
            if current.is_some() && current != context.token {
                return Ok(true);
            }
        }
        let flow = {
            let mut in_flight = self.in_flight();
            in_flight
                .get_or_insert_with(|| {
                    let provider = self.provider.clone();
                    let context = context.clone();
                    async move {
                        let granted = if insufficient_scope { provider.tokens().await? } else { None };
                        let mut options = OAuthFlowOptions::new(context.server_url, OAuthHttp::new(context.client));
                        options.resource_metadata_url = challenge.resource_metadata_url;
                        options.scope = if insufficient_scope {
                            let granted = granted.and_then(|tokens| tokens.scope);
                            step_up_scope(granted.as_deref(), challenge.scope.as_deref())
                        } else {
                            challenge.scope
                        };
                        options.skip_refresh = insufficient_scope;
                        match authorize_mcp(provider.as_ref(), &options).await? {
                            OAuthFlowResult::Redirect => Err(OAuthError::AuthorizationRequired),
                            OAuthFlowResult::Authorized => Ok(()),
                        }
                    }
                    .boxed()
                    .shared()
                })
                .clone()
        };
        let result = flow.clone().await;
        let mut in_flight = self.in_flight();
        if in_flight.as_ref().is_some_and(|current| current.ptr_eq(&flow)) {
            *in_flight = None;
        }
        result?;
        Ok(true)
    }
}
