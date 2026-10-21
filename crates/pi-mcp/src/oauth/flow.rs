//! The MCP authorization flow: discovery, client registration, code exchange, and refresh.

use async_trait::async_trait;
use url::Url;

use super::discovery::{discover_authorization_server_metadata, discover_oauth_server_info, select_resource};
use super::http::OAuthHttp;
use super::requests::{
    AuthorizationRequest, TokenRequest, exchange_authorization_code, refresh_authorization, register_client,
    secure_endpoint, start_authorization,
};
use super::types::{
    AuthorizationServerMetadata, OAuthClientInformation, OAuthClientMetadata, OAuthDiscoveryState, OAuthServerInfo,
    OAuthTokens,
};
use crate::error::OAuthError;

/// A Client ID Metadata Document: an https URL used as `client_id`, and a redirect URI it lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientMetadataDocument {
    pub url: String,
    pub redirect_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidateKind {
    All,
    Client,
    Tokens,
    Verifier,
    Discovery,
}

#[async_trait]
pub trait OAuthClientProvider: Send + Sync {
    fn redirect_url(&self) -> String;
    fn client_metadata(&self) -> OAuthClientMetadata;
    /// A Client ID Metadata Document to identify as instead of registering, asked for when no client
    /// information is stored. The document is not stored.
    fn client_metadata_document(
        &self,
        _metadata: Option<&AuthorizationServerMetadata>,
    ) -> Result<Option<ClientMetadataDocument>, OAuthError> {
        Ok(None)
    }
    async fn state(&self) -> Result<Option<String>, OAuthError> {
        Ok(None)
    }
    async fn client_information(&self) -> Result<Option<OAuthClientInformation>, OAuthError>;
    /// Whether registered clients can be saved; without it the flow cannot register.
    fn can_save_client_information(&self) -> bool {
        true
    }
    async fn save_client_information(&self, information: OAuthClientInformation) -> Result<(), OAuthError>;
    async fn tokens(&self) -> Result<Option<OAuthTokens>, OAuthError>;
    async fn save_tokens(&self, tokens: OAuthTokens) -> Result<(), OAuthError>;
    async fn redirect_to_authorization(&self, url: Url) -> Result<(), OAuthError>;
    async fn save_code_verifier(&self, verifier: String) -> Result<(), OAuthError>;
    async fn code_verifier(&self) -> Result<String, OAuthError>;
    async fn invalidate_credentials(&self, _kind: InvalidateKind) -> Result<(), OAuthError> {
        Ok(())
    }
    async fn save_discovery_state(&self, _state: OAuthDiscoveryState) -> Result<(), OAuthError> {
        Ok(())
    }
    async fn discovery_state(&self) -> Result<Option<OAuthDiscoveryState>, OAuthError> {
        Ok(None)
    }
}

#[derive(Clone)]
pub struct OAuthFlowOptions {
    pub server_url: Url,
    pub authorization_code: Option<String>,
    /// `iss` parameter of the authorization response that delivered the code (RFC 9207).
    pub iss: Option<String>,
    pub scope: Option<String>,
    pub resource_metadata_url: Option<Url>,
    /// Metadata document to use instead of discovery. Trusted as configured; https except on loopback.
    pub authorization_server_metadata_url: Option<Url>,
    pub http: OAuthHttp,
    pub skip_issuer_validation: bool,
    /// Go straight to the authorization redirect instead of refreshing, for example when the server
    /// asks for scopes the grant lacks (a refresh keeps the old scope).
    pub skip_refresh: bool,
}

impl OAuthFlowOptions {
    pub fn new(server_url: Url, http: OAuthHttp) -> Self {
        Self {
            server_url,
            authorization_code: None,
            iss: None,
            scope: None,
            resource_metadata_url: None,
            authorization_server_metadata_url: None,
            http,
            skip_issuer_validation: false,
            skip_refresh: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthFlowResult {
    Authorized,
    Redirect,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

/// A token response without `scope` grants the requested scope (RFC 6749 section 5.1).
fn with_scope(mut tokens: OAuthTokens, scope: Option<&str>) -> OAuthTokens {
    if tokens.scope.is_none() {
        tokens.scope = scope.filter(|scope| !scope.is_empty()).map(str::to_string);
    }
    tokens
}

async fn discover(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
    metadata_url: Option<&Url>,
) -> Result<OAuthServerInfo, OAuthError> {
    let http = &options.http;
    // With a configured metadata URL, discovery is not cached, so changing the URL applies at once.
    let cached = match metadata_url {
        Some(_) => None,
        None => provider.discovery_state().await?.filter(|state| !state.authorization_server_url.is_empty()),
    };
    let discovered = match cached {
        Some(cached) => {
            let metadata = match cached.authorization_server_metadata {
                Some(metadata) => Some(metadata),
                None => {
                    let url = &cached.authorization_server_url;
                    discover_authorization_server_metadata(url, http, options.skip_issuer_validation).await?
                }
            };
            OAuthServerInfo {
                authorization_server_url: cached.authorization_server_url,
                authorization_server_metadata: metadata,
                resource_metadata: cached.resource_metadata,
            }
        }
        None => {
            let resource_metadata_url = options.resource_metadata_url.as_ref();
            discover_oauth_server_info(
                &options.server_url,
                resource_metadata_url,
                metadata_url,
                http,
                options.skip_issuer_validation,
            )
            .await?
        }
    };
    if metadata_url.is_none() {
        let state = OAuthDiscoveryState {
            authorization_server_url: discovered.authorization_server_url.clone(),
            authorization_server_metadata: discovered.authorization_server_metadata.clone(),
            resource_metadata: discovered.resource_metadata.clone(),
            resource_metadata_url: options.resource_metadata_url.as_ref().map(Url::to_string),
        };
        provider.save_discovery_state(state).await?;
    }
    Ok(discovered)
}

async fn run_flow(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
) -> Result<OAuthFlowResult, OAuthError> {
    let metadata_url = match &options.authorization_server_metadata_url {
        Some(url) => Some(secure_endpoint(url.as_str())?),
        None => None,
    };
    let discovered = discover(provider, options, metadata_url.as_ref()).await?;
    let server = discovered.authorization_server_url.as_str();
    let metadata = discovered.authorization_server_metadata.as_ref();
    let resource = select_resource(&options.server_url, discovered.resource_metadata.as_ref())?;
    // An empty scope (for example from `scopes_supported: []`) falls through to the next source.
    let scope = non_empty(options.scope.clone())
        .or_else(|| {
            non_empty(discovered.resource_metadata.as_ref()?.scopes_supported.as_ref().map(|scopes| scopes.join(" ")))
        })
        .or_else(|| non_empty(provider.client_metadata().scope));
    let stored = provider.client_information().await?;
    let document = match stored {
        Some(_) => None,
        None => provider.client_metadata_document(metadata)?,
    };
    if let Some(document) = &document {
        let url = Url::parse(&document.url).map_err(|_| OAuthError::other("Invalid OAuth client metadata URL"))?;
        if url.scheme() != "https" || url.path() == "/" {
            return Err(OAuthError::other("Invalid OAuth client metadata URL"));
        }
    }
    let client = match (stored, &document) {
        (Some(stored), _) => stored,
        (None, Some(document)) => OAuthClientInformation::new(document.url.clone()),
        (None, None) => {
            if options.authorization_code.is_some() {
                return Err(OAuthError::other("OAuth client information is missing during code exchange"));
            }
            if !provider.can_save_client_information() {
                return Err(OAuthError::other("OAuth client information cannot be persisted"));
            }
            let client =
                register_client(server, metadata, &provider.client_metadata(), scope.as_deref(), &options.http).await?;
            provider.save_client_information(client.clone()).await?;
            client
        }
    };
    // The document's redirect URI may differ from the provider's, for example by a server-specific path.
    let redirect_url =
        document.as_ref().map_or_else(|| provider.redirect_url(), |document| document.redirect_url.clone());
    let token_request =
        TokenRequest { metadata, client_information: &client, resource: resource.as_deref(), http: &options.http };
    if let Some(code) = &options.authorization_code {
        // RFC 9207: never send a code from another authorization server to this one. Without metadata
        // there is no issuer to compare with, so any `iss` is rejected.
        let iss_supported =
            metadata.is_some_and(|metadata| metadata.authorization_response_iss_parameter_supported == Some(true));
        let issuer = metadata.map(|metadata| metadata.issuer.as_str());
        if (options.iss.is_some() || iss_supported) && options.iss.as_deref() != issuer {
            return Err(OAuthError::IssuerMismatch {
                expected: issuer.map(str::to_string),
                received: options.iss.clone(),
            });
        }
        let verifier = provider.code_verifier().await?;
        let tokens = exchange_authorization_code(server, &token_request, code, &verifier, &redirect_url).await?;
        provider.save_tokens(with_scope(tokens, scope.as_deref())).await?;
        return Ok(OAuthFlowResult::Authorized);
    }
    let existing = if options.skip_refresh { None } else { provider.tokens().await? };
    if let Some(existing) = existing
        && let Some(refresh_token) = &existing.refresh_token
    {
        match refresh_authorization(server, &token_request, refresh_token).await {
            Ok(tokens) => {
                // A refresh without `scope` keeps the scope of the grant (RFC 6749 section 6).
                provider.save_tokens(with_scope(tokens, existing.scope.as_deref())).await?;
                return Ok(OAuthFlowResult::Authorized);
            }
            Err(error) => {
                if options.http.is_cancelled() || matches!(error, OAuthError::InsecureEndpoint(_)) {
                    return Err(error);
                }
                if error.code().is_some_and(|code| code != "server_error") {
                    return Err(error);
                }
            }
        }
    }
    let state = provider.state().await?;
    let request = AuthorizationRequest {
        metadata,
        client_information: &client,
        redirect_url: &redirect_url,
        scope: scope.as_deref(),
        state: state.as_deref(),
        resource: resource.as_deref(),
    };
    let (authorization_url, verifier) = start_authorization(server, request)?;
    provider.save_code_verifier(verifier).await?;
    provider.redirect_to_authorization(authorization_url).await?;
    Ok(OAuthFlowResult::Redirect)
}

/// Run the flow once more after dropping credentials the server rejected.
pub async fn authorize_mcp(
    provider: &dyn OAuthClientProvider,
    options: &OAuthFlowOptions,
) -> Result<OAuthFlowResult, OAuthError> {
    match run_flow(provider, options).await {
        Err(error) if matches!(error.code(), Some("invalid_client" | "unauthorized_client")) => {
            provider.invalidate_credentials(InvalidateKind::All).await?;
            run_flow(provider, options).await
        }
        Err(error) if error.code() == Some("invalid_grant") => {
            provider.invalidate_credentials(InvalidateKind::Tokens).await?;
            run_flow(provider, options).await
        }
        result => result,
    }
}
