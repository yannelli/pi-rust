//! AWS `external_account` subject tokens: a SigV4-signed `GetCallerIdentity` request,
//! with credentials and region from the environment or the EC2 metadata service.

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde_json::{Value, json};

use super::external::{option, send};
use crate::providers::aws::sigv4::{self, Credentials, SignableRequest};
use crate::simple_options::provider_env_value;

/// Metadata endpoints from `credential_source`.
struct Metadata<'a> {
    region_url: Option<&'a str>,
    credentials_url: Option<&'a str>,
    session_token_url: Option<&'a str>,
}

fn source_str<'a>(source: &'a Value, field: &str) -> Option<&'a str> {
    option(source, field).and_then(Value::as_str).filter(|text| !text.is_empty())
}

/// The URL-encoded JSON serialization of a signed `GetCallerIdentity` request.
pub(super) async fn subject_token(
    source: &Value,
    audience: &str,
    env: &BTreeMap<String, String>,
    client: &reqwest::Client,
) -> Result<String, String> {
    let verification_url = source_str(source, "regional_cred_verification_url");
    let environment_id = source_str(source, "environment_id").unwrap_or_default();
    let version = environment_id
        .strip_prefix("aws")
        .filter(|digits| !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit()));
    let (Some(version), Some(verification_url)) = (version, verification_url) else {
        return Err("No valid AWS \"credential_source\" provided".into());
    };
    if version.parse::<u64>().ok() != Some(1) {
        return Err(format!("aws version \"{version}\" is not supported in the current build."));
    }
    let metadata = Metadata {
        region_url: source_str(source, "region_url"),
        credentials_url: source_str(source, "url"),
        session_token_url: source_str(source, "imdsv2_session_token_url"),
    };
    let region = region(&metadata, env, client).await?;
    let credentials = security_credentials(&metadata, env, client).await?;
    signed_request_token(verification_url, &region, &credentials, audience, SystemTime::now())
}

/// Sign `GetCallerIdentity` and serialize it as `AwsClient.retrieveSubjectToken` does.
pub(super) fn signed_request_token(
    verification_url: &str,
    region: &str,
    credentials: &Credentials,
    audience: &str,
    time: SystemTime,
) -> Result<String, String> {
    let url_text = verification_url.replace("{region}", region);
    let url = reqwest::Url::parse(&url_text).map_err(|error| format!("Invalid URL {url_text}: {error}"))?;
    let service = url.host_str().unwrap_or_default().split('.').next().unwrap_or_default().to_string();
    let request = SignableRequest { method: "POST", url: &url, headers: &[], body: b"" };
    let mut headers = sigv4::sign(&request, credentials, region, &service, time);
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    headers.push(("host".into(), host));
    headers.push(("x-goog-cloud-target-resource".into(), audience.into()));
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let headers: Vec<Value> = headers.into_iter().map(|(key, value)| json!({"key": key, "value": value})).collect();
    Ok(encode_uri_component(&json!({"url": url_text, "method": "POST", "headers": headers}).to_string()))
}

/// JavaScript `encodeURIComponent`.
pub(super) fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

async fn session_token(metadata: &Metadata<'_>, client: &reqwest::Client) -> Result<Option<String>, String> {
    let Some(url) = metadata.session_token_url else { return Ok(None) };
    let request = client.put(url).header("x-aws-ec2-metadata-token-ttl-seconds", "300");
    let reply = send(request).await.map_err(|error| format!("AWS IMDSv2 session token request failed: {error}"))?;
    if !reply.ok() {
        return Err(reply.failure());
    }
    Ok(Some(reply.text))
}

async fn metadata_get(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> Result<super::external::Reply, String> {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.header("x-aws-ec2-metadata-token", token);
    }
    let reply = send(request).await.map_err(|error| format!("AWS metadata request failed: {error}"))?;
    if !reply.ok() {
        return Err(reply.failure());
    }
    Ok(reply)
}

/// `AWS_REGION`, `AWS_DEFAULT_REGION`, or the availability zone from `region_url` minus its letter.
async fn region(
    metadata: &Metadata<'_>,
    env: &BTreeMap<String, String>,
    client: &reqwest::Client,
) -> Result<String, String> {
    if let Some(region) =
        provider_env_value("AWS_REGION", env).or_else(|| provider_env_value("AWS_DEFAULT_REGION", env))
    {
        return Ok(region);
    }
    let token = session_token(metadata, client).await?;
    let Some(url) = metadata.region_url else {
        return Err("Unable to determine AWS region due to missing \"options.credential_source.region_url\"".into());
    };
    let zone = metadata_get(client, url, token.as_deref()).await?.text;
    let mut characters = zone.chars();
    characters.next_back();
    Ok(characters.as_str().to_string())
}

/// Credentials from `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, else the instance role.
async fn security_credentials(
    metadata: &Metadata<'_>,
    env: &BTreeMap<String, String>,
    client: &reqwest::Client,
) -> Result<Credentials, String> {
    if let (Some(id), Some(secret)) =
        (provider_env_value("AWS_ACCESS_KEY_ID", env), provider_env_value("AWS_SECRET_ACCESS_KEY", env))
    {
        let mut credentials = Credentials::new(id, secret);
        credentials.session_token = provider_env_value("AWS_SESSION_TOKEN", env);
        return Ok(credentials);
    }
    let token = session_token(metadata, client).await?;
    let Some(url) = metadata.credentials_url else {
        return Err("Unable to determine AWS role name due to missing \"options.credential_source.url\"".into());
    };
    let role = metadata_get(client, url, token.as_deref()).await?.text;
    let reply = metadata_get(client, &format!("{url}/{role}"), token.as_deref()).await?;
    let field = |name: &str| reply.json[name].as_str().unwrap_or_default().to_string();
    let mut credentials = Credentials::new(field("AccessKeyId"), field("SecretAccessKey"));
    credentials.session_token = Some(field("Token")).filter(|token| !token.is_empty());
    Ok(credentials)
}
