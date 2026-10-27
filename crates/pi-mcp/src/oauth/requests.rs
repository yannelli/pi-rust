//! Requests to the authorization server: authorization URLs, tokens, and client registration.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use url::Url;

use super::http::OAuthHttp;
use super::pkce;
use super::types::{
    AuthorizationServerMetadata, OAuthClientInformation, OAuthClientMetadata, OAuthTokens, is_http_url,
    parse_client_information, parse_oauth_tokens,
};
use crate::error::OAuthError;

pub(crate) fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// Refuse to send credentials over plain HTTP, except to loopback hosts.
pub fn secure_endpoint(value: &str) -> Result<Url, OAuthError> {
    let url = Url::parse(value).map_err(|_| OAuthError::other(format!("Invalid URL {value}")))?;
    if url.scheme() != "https" && !is_loopback(url.host_str().unwrap_or_default()) {
        return Err(OAuthError::InsecureEndpoint(url.to_string()));
    }
    Ok(url)
}

/// `new URL(path, base)` for an absolute path.
fn endpoint(base: &str, path: &str) -> Result<Url, OAuthError> {
    Url::parse(base)
        .and_then(|base| base.join(path))
        .map_err(|_| OAuthError::other(format!("Invalid authorization server URL {base}")))
}

/// Set a query parameter, replacing earlier values, like `URLSearchParams.set`.
pub(crate) fn set_query(url: &mut Url, key: &str, value: &str) {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| name != key)
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    let mut query = url.query_pairs_mut();
    query.clear();
    for (name, existing) in &pairs {
        query.append_pair(name, existing);
    }
    query.append_pair(key, value);
}

/// The OpenID Connect `application_type` for `redirect_uris` (MCP SEP-837): loopback hosts and custom
/// schemes are native clients (RFC 8252), which `web` would reject.
fn application_type(redirect_uris: &[String]) -> &'static str {
    let native = redirect_uris.iter().filter_map(|uri| Url::parse(uri).ok()).any(|url| {
        (url.scheme() != "http" && url.scheme() != "https") || is_loopback(url.host_str().unwrap_or_default())
    });
    if native { "native" } else { "web" }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientAuthMethod {
    Basic,
    Post,
    None,
}

fn select_client_auth_method(information: &OAuthClientInformation, supported: &[String]) -> ClientAuthMethod {
    let parse = |method: &str| match method {
        "client_secret_basic" => Some(ClientAuthMethod::Basic),
        "client_secret_post" => Some(ClientAuthMethod::Post),
        "none" => Some(ClientAuthMethod::None),
        _ => None,
    };
    let supports = |method: &str| supported.iter().any(|entry| entry == method);
    if let Some(hinted) = information.token_endpoint_auth_method.as_deref()
        && let Some(method) = parse(hinted)
        && (supported.is_empty() || supports(hinted))
    {
        return method;
    }
    let secret = information.client_secret.is_some();
    if supported.is_empty() {
        return if secret { ClientAuthMethod::Basic } else { ClientAuthMethod::None };
    }
    if secret && supports("client_secret_basic") {
        return ClientAuthMethod::Basic;
    }
    if secret && supports("client_secret_post") {
        return ClientAuthMethod::Post;
    }
    if supports("none") {
        return ClientAuthMethod::None;
    }
    if secret { ClientAuthMethod::Post } else { ClientAuthMethod::None }
}

pub struct AuthorizationRequest<'a> {
    pub metadata: Option<&'a AuthorizationServerMetadata>,
    pub client_information: &'a OAuthClientInformation,
    pub redirect_url: &'a str,
    pub scope: Option<&'a str>,
    pub state: Option<&'a str>,
    pub resource: Option<&'a str>,
}

/// The authorization URL to open, and the PKCE verifier to keep for the code exchange.
pub fn start_authorization(
    authorization_server_url: &str,
    request: AuthorizationRequest<'_>,
) -> Result<(Url, String), OAuthError> {
    if let Some(metadata) = request.metadata {
        if !metadata.response_types_supported.iter().any(|kind| kind == "code") {
            return Err(OAuthError::other("Authorization server does not support authorization codes"));
        }
        if let Some(methods) = &metadata.code_challenge_methods_supported
            && !methods.iter().any(|method| method == "S256")
        {
            return Err(OAuthError::other("Authorization server does not support PKCE S256"));
        }
    }
    let mut url = match request.metadata {
        Some(metadata) => Url::parse(&metadata.authorization_endpoint)
            .map_err(|_| OAuthError::other("Invalid authorization endpoint"))?,
        None => endpoint(authorization_server_url, "/authorize")?,
    };
    // Callers open this URL with the platform URL handler, which also launches files and custom protocols.
    if !is_http_url(&url) {
        return Err(OAuthError::other(format!("Invalid authorization endpoint {url}")));
    }
    let pkce = pkce::generate();
    set_query(&mut url, "response_type", "code");
    set_query(&mut url, "client_id", &request.client_information.client_id);
    set_query(&mut url, "code_challenge", &pkce.challenge);
    set_query(&mut url, "code_challenge_method", "S256");
    set_query(&mut url, "redirect_uri", request.redirect_url);
    if let Some(state) = request.state.filter(|state| !state.is_empty()) {
        set_query(&mut url, "state", state);
    }
    if let Some(scope) = request.scope.filter(|scope| !scope.is_empty()) {
        set_query(&mut url, "scope", scope);
        if scope.split_whitespace().any(|entry| entry == "offline_access") {
            set_query(&mut url, "prompt", "consent");
        }
    }
    if let Some(resource) = request.resource {
        set_query(&mut url, "resource", resource);
    }
    Ok((url, pkce.verifier))
}

#[derive(Clone)]
pub struct TokenRequest<'a> {
    pub metadata: Option<&'a AuthorizationServerMetadata>,
    pub client_information: &'a OAuthClientInformation,
    pub resource: Option<&'a str>,
    pub http: &'a OAuthHttp,
}

async fn token_request(
    authorization_server_url: &str,
    options: &TokenRequest<'_>,
    mut params: Vec<(&str, String)>,
) -> Result<OAuthTokens, OAuthError> {
    let url = match options.metadata {
        Some(metadata) => secure_endpoint(&metadata.token_endpoint)?,
        None => secure_endpoint(endpoint(authorization_server_url, "/token")?.as_str())?,
    };
    if let Some(resource) = options.resource {
        params.push(("resource", resource.to_string()));
    }
    let information = options.client_information;
    let supported = options.metadata.and_then(|metadata| metadata.token_endpoint_auth_methods_supported.clone());
    let mut request = options.http.client.post(url).header("accept", "application/json");
    match select_client_auth_method(information, &supported.unwrap_or_default()) {
        ClientAuthMethod::Basic => {
            let secret = information.client_secret.as_deref().unwrap_or_default();
            let credentials = STANDARD.encode(format!("{}:{secret}", information.client_id));
            request = request.header("authorization", format!("Basic {credentials}"));
        }
        method => {
            params.push(("client_id", information.client_id.clone()));
            if let (ClientAuthMethod::Post, Some(secret)) = (method, &information.client_secret) {
                params.push(("client_secret", secret.clone()));
            }
        }
    }
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(&params).finish();
    let request = request.header("content-type", "application/x-www-form-urlencoded").body(body);
    let response = options.http.execute(request).await?;
    let value: Option<Value> = serde_json::from_str(&response.text).ok();
    // Servers may report OAuth errors with any status, so check the body before the status.
    if let Some(error) = value.as_ref().and_then(|value| value.get("error")).and_then(Value::as_str) {
        let field =
            |key: &str| value.as_ref().and_then(|value| value.get(key)).and_then(Value::as_str).map(str::to_string);
        return Err(OAuthError::Server {
            code: error.to_string(),
            message: field("error_description").filter(|text| !text.is_empty()).unwrap_or_else(|| error.to_string()),
            error_uri: field("error_uri"),
        });
    }
    if !response.ok() {
        return Err(OAuthError::server("server_error", format!("HTTP {}: {}", response.status, response.text)));
    }
    parse_oauth_tokens(&value.unwrap_or(Value::Null))
}

pub async fn register_client(
    authorization_server_url: &str,
    metadata: Option<&AuthorizationServerMetadata>,
    client_metadata: &OAuthClientMetadata,
    scope: Option<&str>,
    http: &OAuthHttp,
) -> Result<OAuthClientInformation, OAuthError> {
    let url = match metadata {
        Some(metadata) => match &metadata.registration_endpoint {
            Some(endpoint) => Url::parse(endpoint).map_err(|_| OAuthError::other("Invalid registration endpoint"))?,
            None => return Err(OAuthError::other("Authorization server does not support dynamic client registration")),
        },
        None => endpoint(authorization_server_url, "/register")?,
    };
    let mut body = json!(client_metadata);
    if client_metadata.application_type.is_none() {
        body["application_type"] = json!(application_type(&client_metadata.redirect_uris));
    }
    if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
        body["scope"] = json!(scope);
    }
    let request = http
        .client
        .post(url)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body(body.to_string());
    let response = http.execute(request).await?;
    if !response.ok() {
        return Err(OAuthError::Registration { status: response.status, body: response.text });
    }
    parse_client_information(&response.json()?)
}

pub async fn exchange_authorization_code(
    authorization_server_url: &str,
    options: &TokenRequest<'_>,
    code: &str,
    code_verifier: &str,
    redirect_url: &str,
) -> Result<OAuthTokens, OAuthError> {
    let params = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("code_verifier", code_verifier.to_string()),
        ("redirect_uri", redirect_url.to_string()),
    ];
    token_request(authorization_server_url, options, params).await
}

/// Refresh tokens. A response without a refresh token keeps the old one.
pub async fn refresh_authorization(
    authorization_server_url: &str,
    options: &TokenRequest<'_>,
    refresh_token: &str,
) -> Result<OAuthTokens, OAuthError> {
    let params = vec![("grant_type", "refresh_token".to_string()), ("refresh_token", refresh_token.to_string())];
    let mut tokens = token_request(authorization_server_url, options, params).await?;
    tokens.refresh_token.get_or_insert_with(|| refresh_token.to_string());
    Ok(tokens)
}

/// Scopes for a step-up authorization: the challenged scopes plus those granted so far, since a
/// challenge may list only the missing scopes (SEP-2350). `None` without challenged scopes.
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
    let challenged = challenged.filter(|scope| !scope.is_empty())?;
    let mut scopes: Vec<&str> = Vec::new();
    for scope in granted.unwrap_or_default().split_whitespace().chain(challenged.split_whitespace()) {
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    Some(scopes.join(" "))
}
