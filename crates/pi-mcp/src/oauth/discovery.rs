//! Protected resource (RFC 9728) and authorization server (RFC 8414, OIDC) discovery.

use url::Url;

use super::http::{OAuthHttp, OAuthResponse};
use super::types::{
    AuthorizationServerMetadata, OAuthChallenge, OAuthServerInfo, ProtectedResourceMetadata,
    parse_authorization_server_metadata, parse_protected_resource_metadata,
};
use crate::error::OAuthError;
use crate::types::LATEST_PROTOCOL_VERSION;

/// 4xx and 502 mean "not here", so discovery tries the next candidate.
fn is_discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

/// Path suffix for `/.well-known/<kind><path>`; empty for the root path.
fn path_suffix(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

fn origin_url(url: &Url, path: &str) -> Result<Url, OAuthError> {
    Url::parse(&format!("{}{path}", url.origin().ascii_serialization()))
        .map_err(|error| OAuthError::other(format!("Invalid URL: {error}")))
}

/// Value of one challenge parameter. An empty value carries no information, so it counts as absent.
fn challenge_field(header: &str, name: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let pattern = format!("{name}=");
    let index = lower
        .match_indices(&pattern)
        .map(|(index, _)| index)
        .find(|&index| index == 0 || matches!(lower.as_bytes()[index - 1], b',' | b' ' | b'\t' | b'\r' | b'\n'))?;
    let rest = &header[index + pattern.len()..];
    let quoted = rest.strip_prefix('"').and_then(|rest| rest.find('"').map(|end| rest[..end].to_string()));
    let value = quoted.unwrap_or_else(|| {
        rest.chars().take_while(|character| !character.is_whitespace() && *character != ',').collect()
    });
    (!value.is_empty()).then_some(value)
}

/// The bearer (or DPoP) challenge of a `WWW-Authenticate` header.
pub fn parse_www_authenticate(header: Option<&str>) -> OAuthChallenge {
    let Some(header) = header else { return OAuthChallenge::default() };
    let scheme = header.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if scheme != "bearer" && scheme != "dpop" {
        return OAuthChallenge::default();
    }
    OAuthChallenge {
        resource_metadata_url: challenge_field(header, "resource_metadata").and_then(|url| Url::parse(&url).ok()),
        scope: challenge_field(header, "scope"),
        error: challenge_field(header, "error"),
        error_description: challenge_field(header, "error_description"),
    }
}

async fn fetch_metadata(url: &Url, http: &OAuthHttp) -> Result<OAuthResponse, OAuthError> {
    let request = http
        .client
        .get(url.clone())
        .header("accept", "application/json")
        .header("mcp-protocol-version", LATEST_PROTOCOL_VERSION);
    http.execute(request).await
}

pub async fn discover_protected_resource_metadata(
    server_url: &Url,
    resource_metadata_url: Option<&Url>,
    http: &OAuthHttp,
) -> Result<ProtectedResourceMetadata, OAuthError> {
    let default_url =
        || origin_url(server_url, &format!("/.well-known/oauth-protected-resource{}", path_suffix(server_url.path())));
    let url = match resource_metadata_url {
        Some(url) => url.clone(),
        None => default_url()?,
    };
    let mut response = fetch_metadata(&url, http).await?;
    if resource_metadata_url.is_none() && server_url.path() != "/" && is_discovery_miss(response.status) {
        response = fetch_metadata(&origin_url(server_url, "/.well-known/oauth-protected-resource")?, http).await?;
    }
    if !response.ok() {
        return Err(OAuthError::other(format!("HTTP {} loading OAuth protected resource metadata", response.status)));
    }
    parse_protected_resource_metadata(&response.json()?)
}

/// Candidate metadata URLs of an issuer, OAuth first, then OpenID Connect.
pub fn authorization_server_discovery_urls(authorization_server_url: &Url) -> Result<Vec<Url>, OAuthError> {
    let path = path_suffix(authorization_server_url.path());
    let mut urls = vec![
        origin_url(authorization_server_url, &format!("/.well-known/oauth-authorization-server{path}"))?,
        origin_url(authorization_server_url, &format!("/.well-known/openid-configuration{path}"))?,
    ];
    if !path.is_empty() {
        urls.push(origin_url(authorization_server_url, &format!("{path}/.well-known/openid-configuration"))?);
    }
    Ok(urls)
}

pub async fn discover_authorization_server_metadata(
    authorization_server_url: &str,
    http: &OAuthHttp,
    skip_issuer_validation: bool,
) -> Result<Option<AuthorizationServerMetadata>, OAuthError> {
    let issuer_url = Url::parse(authorization_server_url)
        .map_err(|_| OAuthError::other(format!("Invalid authorization server URL {authorization_server_url}")))?;
    for url in authorization_server_discovery_urls(&issuer_url)? {
        let response = fetch_metadata(&url, http).await?;
        if !response.ok() {
            if is_discovery_miss(response.status) {
                continue;
            }
            return Err(OAuthError::other(format!(
                "HTTP {} loading authorization server metadata from {url}",
                response.status
            )));
        }
        let metadata = parse_authorization_server_metadata(&response.json()?)?;
        // URL parsing adds a trailing slash to bare origins, so compare without one on either side.
        let trim = |value: &str| value.strip_suffix('/').unwrap_or(value).to_string();
        if !skip_issuer_validation && trim(&metadata.issuer) != trim(authorization_server_url) {
            return Err(OAuthError::IssuerMismatch {
                expected: authorization_server_url.to_string(),
                received: Some(metadata.issuer),
            });
        }
        return Ok(Some(metadata));
    }
    Ok(None)
}

/// Discover the authorization server of an MCP server. A configured metadata document is trusted
/// as it is, so its issuer is not checked.
pub async fn discover_oauth_server_info(
    server_url: &Url,
    resource_metadata_url: Option<&Url>,
    authorization_server_metadata_url: Option<&Url>,
    http: &OAuthHttp,
    skip_issuer_validation: bool,
) -> Result<OAuthServerInfo, OAuthError> {
    let resource_metadata = match discover_protected_resource_metadata(server_url, resource_metadata_url, http).await {
        Ok(metadata) => Some(metadata),
        Err(error @ (OAuthError::Network(_) | OAuthError::Aborted)) => return Err(error),
        Err(_) => None,
    };
    if let Some(url) = authorization_server_metadata_url {
        let response = fetch_metadata(url, http).await?;
        if !response.ok() {
            return Err(OAuthError::other(format!(
                "HTTP {} loading authorization server metadata from {url}",
                response.status
            )));
        }
        let metadata = parse_authorization_server_metadata(&response.json()?)?;
        return Ok(OAuthServerInfo {
            authorization_server_url: metadata.issuer.clone(),
            authorization_server_metadata: Some(metadata),
            resource_metadata,
        });
    }
    let authorization_server_url = resource_metadata
        .as_ref()
        .and_then(|metadata| metadata.authorization_servers.as_ref()?.first().cloned())
        .map_or_else(|| origin_url(server_url, "/").map(String::from), Ok)?;
    let metadata =
        discover_authorization_server_metadata(&authorization_server_url, http, skip_issuer_validation).await?;
    Ok(OAuthServerInfo { authorization_server_url, authorization_server_metadata: metadata, resource_metadata })
}

/// The server URL without its fragment, as the `resource` parameter names it.
pub fn resource_url_from_server_url(server_url: &Url) -> Url {
    let mut url = server_url.clone();
    url.set_fragment(None);
    url
}

/// The `resource` to request: the metadata's resource, which must cover the MCP server URL.
pub fn select_resource(
    server_url: &Url,
    metadata: Option<&ProtectedResourceMetadata>,
) -> Result<Option<String>, OAuthError> {
    let Some(metadata) = metadata else { return Ok(None) };
    let requested = resource_url_from_server_url(server_url);
    let mismatch =
        || OAuthError::other(format!("Protected resource {} does not match MCP server {requested}", metadata.resource));
    let configured = Url::parse(&metadata.resource).map_err(|_| mismatch())?;
    if requested.origin() != configured.origin() {
        return Err(mismatch());
    }
    let with_slash = |path: &str| if path.ends_with('/') { path.to_string() } else { format!("{path}/") };
    if !with_slash(requested.path()).starts_with(&with_slash(configured.path())) {
        return Err(mismatch());
    }
    Ok(Some(metadata.resource.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer_challenges() {
        let challenge = parse_www_authenticate(Some(
            r#"Bearer resource_metadata="https://a.example/.well-known/oauth-protected-resource/mcp", scope="", error="insufficient_scope""#,
        ));
        assert_eq!(
            challenge.resource_metadata_url.as_ref().map(Url::as_str),
            Some("https://a.example/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(challenge.scope, None);
        assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(parse_www_authenticate(Some("Bearer scope=read,error=x")).scope.as_deref(), Some("read"));
        assert_eq!(parse_www_authenticate(Some("Basic realm=x")), OAuthChallenge::default());
    }

    #[test]
    fn builds_discovery_urls_and_selects_resources() {
        let urls = authorization_server_discovery_urls(&Url::parse("https://a.example/tenant/").unwrap()).unwrap();
        let urls: Vec<&str> = urls.iter().map(Url::as_str).collect();
        assert_eq!(
            urls,
            [
                "https://a.example/.well-known/oauth-authorization-server/tenant",
                "https://a.example/.well-known/openid-configuration/tenant",
                "https://a.example/tenant/.well-known/openid-configuration",
            ]
        );
        let server = Url::parse("https://a.example/mcp/v1#x").unwrap();
        let metadata = |resource: &str| ProtectedResourceMetadata { resource: resource.into(), ..Default::default() };
        assert_eq!(
            select_resource(&server, Some(&metadata("https://a.example/mcp"))).unwrap().as_deref(),
            Some("https://a.example/mcp")
        );
        assert!(select_resource(&server, Some(&metadata("https://a.example/other"))).is_err());
        assert!(select_resource(&server, Some(&metadata("https://b.example/mcp"))).is_err());
    }
}
