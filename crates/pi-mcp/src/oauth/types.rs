//! OAuth metadata, tokens, and client information, with the lenient parsing servers need.
//! Field names are the OAuth wire names, which `mcp-auth.json` stores as they are.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::error::OAuthError;
use crate::types::JsonObject;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: JsonObject,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    pub response_types_supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_types_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_metadata_document_supported: Option<bool>,
    /// Whether authorization responses carry an `iss` parameter (RFC 9207).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_response_iss_parameter_supported: Option<bool>,
    #[serde(flatten)]
    pub extra: JsonObject,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// Client metadata for dynamic client registration (RFC 7591).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthClientMetadata {
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_types: Option<Vec<String>>,
    /// OpenID Connect client type, `native` or `web`; derived from `redirect_uris` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// `client_uri`, `logo_uri`, `contacts`, `software_id`, and the other optional fields.
    #[serde(flatten)]
    pub extra: JsonObject,
}

/// A registered or configured client. Registered clients also carry their metadata.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthClientInformation {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uris: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(flatten)]
    pub extra: JsonObject,
}

impl OAuthClientInformation {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self { client_id: client_id.into(), ..Self::default() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthDiscoveryState {
    pub authorization_server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_metadata: Option<ProtectedResourceMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_metadata_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OAuthServerInfo {
    pub authorization_server_url: String,
    pub authorization_server_metadata: Option<AuthorizationServerMetadata>,
    pub resource_metadata: Option<ProtectedResourceMetadata>,
}

/// The parts of a `WWW-Authenticate` bearer challenge sign-in uses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OAuthChallenge {
    pub resource_metadata_url: Option<Url>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

fn invalid(name: &str) -> OAuthError {
    OAuthError::other(format!("Invalid {name}"))
}

/// `null` and `""` count as absent: servers send them for fields they have no value for.
fn absent(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null)) || value.and_then(Value::as_str) == Some("")
}

fn required_string(value: Option<&Value>, name: &str) -> Result<String, OAuthError> {
    value.and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string).ok_or_else(|| invalid(name))
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<String>, OAuthError> {
    if absent(value) { Ok(None) } else { required_string(value, name).map(Some) }
}

fn optional_strings(value: Option<&Value>, name: &str) -> Result<Option<Vec<String>>, OAuthError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string).ok_or_else(|| invalid(name)))
            .collect::<Result<_, _>>()
            .map(Some),
        Some(_) => Err(invalid(name)),
    }
}

fn safe_url(value: Option<&Value>, name: &str) -> Result<String, OAuthError> {
    let text = required_string(value, name)?;
    let url = Url::parse(&text).map_err(|_| invalid(name))?;
    if matches!(url.scheme(), "javascript" | "data" | "vbscript") {
        return Err(invalid(name));
    }
    Ok(text)
}

fn optional_url(value: Option<&Value>, name: &str) -> Result<Option<String>, OAuthError> {
    if absent(value) { Ok(None) } else { safe_url(value, name).map(Some) }
}

fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, OAuthError> {
    value.as_object().ok_or_else(|| invalid(name))
}

/// Fields other than `known`, for metadata that keeps what the server sent.
fn rest(input: &Map<String, Value>, known: &[&str]) -> JsonObject {
    input
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn parse_protected_resource_metadata(value: &Value) -> Result<ProtectedResourceMetadata, OAuthError> {
    let input = object(value, "OAuth protected resource metadata")?;
    let authorization_servers = optional_strings(input.get("authorization_servers"), "authorization_servers")?
        .map(|urls| {
            urls.into_iter().map(|url| safe_url(Some(&Value::String(url)), "authorization server URL")).collect()
        })
        .transpose()?;
    Ok(ProtectedResourceMetadata {
        resource: safe_url(input.get("resource"), "OAuth protected resource metadata resource")?,
        authorization_servers,
        scopes_supported: optional_strings(input.get("scopes_supported"), "scopes_supported")?,
        extra: rest(input, &["resource", "authorization_servers", "scopes_supported"]),
    })
}

pub fn parse_authorization_server_metadata(value: &Value) -> Result<AuthorizationServerMetadata, OAuthError> {
    let input = object(value, "authorization server metadata")?;
    let response_types = optional_strings(input.get("response_types_supported"), "response_types_supported")?
        .ok_or_else(|| invalid("response_types_supported"))?;
    let strings = |key: &str| optional_strings(input.get(key), key);
    let boolean = |key: &str| input.get(key).and_then(Value::as_bool);
    Ok(AuthorizationServerMetadata {
        issuer: safe_url(input.get("issuer"), "authorization server issuer")?,
        authorization_endpoint: safe_url(input.get("authorization_endpoint"), "authorization endpoint")?,
        token_endpoint: safe_url(input.get("token_endpoint"), "token endpoint")?,
        registration_endpoint: optional_url(input.get("registration_endpoint"), "registration endpoint")?,
        scopes_supported: strings("scopes_supported")?,
        response_types_supported: response_types,
        grant_types_supported: strings("grant_types_supported")?,
        token_endpoint_auth_methods_supported: strings("token_endpoint_auth_methods_supported")?,
        code_challenge_methods_supported: strings("code_challenge_methods_supported")?,
        client_id_metadata_document_supported: boolean("client_id_metadata_document_supported"),
        authorization_response_iss_parameter_supported: boolean("authorization_response_iss_parameter_supported"),
        extra: rest(
            input,
            &[
                "issuer",
                "authorization_endpoint",
                "token_endpoint",
                "registration_endpoint",
                "scopes_supported",
                "response_types_supported",
                "grant_types_supported",
                "token_endpoint_auth_methods_supported",
                "code_challenge_methods_supported",
                "client_id_metadata_document_supported",
                "authorization_response_iss_parameter_supported",
            ],
        ),
    })
}

/// `Number(value)` of JavaScript, for `expires_in` sent as a string.
fn to_number(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        Value::String(text) if text.trim().is_empty() => 0.0,
        Value::String(text) => text.trim().parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

pub fn parse_oauth_tokens(value: &Value) -> Result<OAuthTokens, OAuthError> {
    let input = object(value, "OAuth token response")?;
    // `Number(null)` is 0, which would mark the token as expired at once.
    let expires_in = if absent(input.get("expires_in")) { None } else { input.get("expires_in").map(to_number) };
    if expires_in.is_some_and(|expires| !expires.is_finite()) {
        return Err(invalid("expires_in"));
    }
    Ok(OAuthTokens {
        access_token: required_string(input.get("access_token"), "access_token")?,
        token_type: required_string(input.get("token_type"), "token_type")?,
        expires_in,
        scope: optional_string(input.get("scope"), "scope")?,
        refresh_token: optional_string(input.get("refresh_token"), "refresh_token")?,
        id_token: optional_string(input.get("id_token"), "id_token")?,
    })
}

pub fn parse_client_information(value: &Value) -> Result<OAuthClientInformation, OAuthError> {
    let input = object(value, "OAuth client registration response")?;
    let token_endpoint_auth_method =
        input.get("token_endpoint_auth_method").and_then(Value::as_str).map(str::to_string);
    Ok(OAuthClientInformation {
        client_id: required_string(input.get("client_id"), "client_id")?,
        client_secret: optional_string(input.get("client_secret"), "client_secret")?,
        client_id_issued_at: input.get("client_id_issued_at").and_then(Value::as_f64),
        client_secret_expires_at: input.get("client_secret_expires_at").and_then(Value::as_f64),
        redirect_uris: Some(optional_strings(input.get("redirect_uris"), "redirect_uris")?.unwrap_or_default()),
        token_endpoint_auth_method,
        extra: rest(
            input,
            &[
                "client_id",
                "client_secret",
                "client_id_issued_at",
                "client_secret_expires_at",
                "redirect_uris",
                "token_endpoint_auth_method",
            ],
        )
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .collect(),
    })
}
