//! Credential sources reached over HTTP: STS role assumption, ECS/EKS container
//! endpoints, EC2 instance metadata (IMDSv2), and the IAM Identity Center portal.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::endpoint::service_host;
use super::sigv4::{Credentials, SignableRequest, parse_rfc3339, sign, uri_encode};
use crate::providers::provider_env::provider_env_value;
use crate::registry::http_client;

const METADATA_TIMEOUT: Duration = Duration::from_secs(1);
const STS_TIMEOUT: Duration = Duration::from_secs(30);

/// Parameters of an STS `AssumeRole` or `AssumeRoleWithWebIdentity` call.
pub struct RoleRequest<'a> {
    pub role_arn: &'a str,
    pub session_name: String,
    pub region: &'a str,
    pub external_id: Option<&'a str>,
    pub duration_seconds: Option<&'a str>,
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(key, value)| format!("{key}={}", uri_encode(value))).collect::<Vec<_>>().join("&")
}

fn sts_url(region: &str, env: &BTreeMap<String, String>) -> String {
    provider_env_value("AWS_ENDPOINT_URL_STS", env)
        .unwrap_or_else(|| format!("https://{}/", service_host("sts", region)))
}

/// `sts:AssumeRole` signed with `source` credentials.
pub async fn assume_role(
    request: &RoleRequest<'_>,
    source: &Credentials,
    env: &BTreeMap<String, String>,
) -> Result<Credentials, String> {
    let mut pairs = vec![
        ("Action", "AssumeRole"),
        ("Version", "2011-06-15"),
        ("RoleArn", request.role_arn),
        ("RoleSessionName", request.session_name.as_str()),
        ("DurationSeconds", request.duration_seconds.unwrap_or("3600")),
    ];
    if let Some(external_id) = request.external_id {
        pairs.push(("ExternalId", external_id));
    }
    let body = form(&pairs);
    let url =
        reqwest::Url::parse(&sts_url(request.region, env)).map_err(|error| format!("Invalid STS URL: {error}"))?;
    let headers = vec![("content-type".to_string(), "application/x-www-form-urlencoded".to_string())];
    let signable = SignableRequest { method: "POST", url: &url, headers: &headers, body: body.as_bytes() };
    let mut builder = http_client().post(url.clone()).timeout(STS_TIMEOUT).body(body.clone());
    for (name, value) in
        headers.iter().cloned().chain(sign(&signable, source, request.region, "sts", SystemTime::now()))
    {
        builder = builder.header(name, value);
    }
    parse_sts_response(send_text(builder).await?)
}

/// `sts:AssumeRoleWithWebIdentity`, which needs no signature.
pub async fn assume_role_with_web_identity(
    request: &RoleRequest<'_>,
    token: &str,
    env: &BTreeMap<String, String>,
) -> Result<Credentials, String> {
    let body = form(&[
        ("Action", "AssumeRoleWithWebIdentity"),
        ("Version", "2011-06-15"),
        ("RoleArn", request.role_arn),
        ("RoleSessionName", request.session_name.as_str()),
        ("WebIdentityToken", token),
    ]);
    let builder = http_client()
        .post(sts_url(request.region, env))
        .timeout(STS_TIMEOUT)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body);
    parse_sts_response(send_text(builder).await?)
}

async fn send_text(builder: reqwest::RequestBuilder) -> Result<String, String> {
    let response = builder.send().await.map_err(|error| format!("Credential request failed: {error}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|error| format!("Credential response failed: {error}"))?;
    if !status.is_success() {
        let message = xml_tag(&text, "Message").unwrap_or_else(|| text.trim().chars().take(300).collect());
        return Err(format!("Credential request failed with HTTP {}: {message}", status.as_u16()));
    }
    Ok(text)
}

/// The text of the first `<tag>` element, with XML entities decoded.
pub fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    let text = &xml[start..end];
    Some(
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

fn parse_sts_response(xml: String) -> Result<Credentials, String> {
    let field = |tag: &str| xml_tag(&xml, tag).ok_or_else(|| format!("STS response is missing {tag}"));
    Ok(Credentials {
        access_key_id: field("AccessKeyId")?,
        secret_access_key: field("SecretAccessKey")?,
        session_token: Some(field("SessionToken")?),
        expiration: xml_tag(&xml, "Expiration").and_then(|text| parse_rfc3339(&text)),
    })
}

/// Credentials from a JSON document with `AccessKeyId`, `SecretAccessKey`, `Token` or
/// `SessionToken`, and `Expiration` (container, IMDS, and `credential_process` output).
pub fn credentials_from_json(value: &Value) -> Result<Credentials, String> {
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    Ok(Credentials {
        access_key_id: field("AccessKeyId").ok_or("Credential response is missing AccessKeyId")?,
        secret_access_key: field("SecretAccessKey").ok_or("Credential response is missing SecretAccessKey")?,
        session_token: field("Token").or_else(|| field("SessionToken")),
        expiration: field("Expiration").and_then(|text| parse_rfc3339(&text)),
    })
}

/// ECS task role or EKS pod identity from `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` or `_FULL_URI`.
pub async fn container_credentials(env: &BTreeMap<String, String>) -> Result<Credentials, String> {
    let url = match (
        provider_env_value("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", env),
        provider_env_value("AWS_CONTAINER_CREDENTIALS_FULL_URI", env),
    ) {
        (Some(relative), _) => format!("http://169.254.170.2{relative}"),
        (None, Some(full)) => full,
        (None, None) => return Err("Container credentials are not configured".into()),
    };
    let token = match provider_env_value("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", env) {
        Some(path) => Some(
            std::fs::read_to_string(&path)
                .map_err(|error| format!("Cannot read container authorization token {path}: {error}"))?
                .trim()
                .to_string(),
        ),
        None => provider_env_value("AWS_CONTAINER_AUTHORIZATION_TOKEN", env),
    };
    let mut builder = http_client().get(url).timeout(METADATA_TIMEOUT);
    if let Some(token) = token {
        builder = builder.header("authorization", token);
    }
    let text = send_text(builder).await?;
    credentials_from_json(
        &serde_json::from_str(&text).map_err(|error| format!("Invalid container credentials: {error}"))?,
    )
}

/// EC2 instance role credentials through IMDSv2.
pub async fn instance_metadata_credentials(env: &BTreeMap<String, String>) -> Result<Credentials, String> {
    if provider_env_value("AWS_EC2_METADATA_DISABLED", env).is_some_and(|value| value != "false") {
        return Err("EC2 instance metadata access is disabled".into());
    }
    let base = provider_env_value("AWS_EC2_METADATA_SERVICE_ENDPOINT", env)
        .unwrap_or_else(|| "http://169.254.169.254".into())
        .trim_end_matches('/')
        .to_string();
    let token = send_text(
        http_client()
            .put(format!("{base}/latest/api/token"))
            .timeout(METADATA_TIMEOUT)
            .header("x-aws-ec2-metadata-token-ttl-seconds", "21600"),
    )
    .await?;
    let get = |path: String| {
        http_client().get(format!("{base}{path}")).timeout(METADATA_TIMEOUT).header("x-aws-ec2-metadata-token", &token)
    };
    let roles = send_text(get("/latest/meta-data/iam/security-credentials/".into())).await?;
    let role = roles.lines().map(str::trim).find(|line| !line.is_empty()).ok_or("No EC2 instance role is attached")?;
    let text = send_text(get(format!("/latest/meta-data/iam/security-credentials/{role}"))).await?;
    credentials_from_json(
        &serde_json::from_str(&text).map_err(|error| format!("Invalid instance credentials: {error}"))?,
    )
}

/// Role credentials from the IAM Identity Center portal with a cached SSO access token.
pub async fn sso_role_credentials(
    region: &str,
    account_id: &str,
    role_name: &str,
    access_token: &str,
) -> Result<Credentials, String> {
    let url = format!(
        "https://portal.sso.{region}.{}/federation/credentials?account_id={}&role_name={}",
        super::endpoint::dns_suffix(region),
        uri_encode(account_id),
        uri_encode(role_name)
    );
    let builder = http_client().get(url).timeout(STS_TIMEOUT).header("x-amz-sso_bearer_token", access_token);
    let text = send_text(builder).await?;
    let value: Value = serde_json::from_str(&text).map_err(|error| format!("Invalid SSO credentials: {error}"))?;
    let role = &value["roleCredentials"];
    let field = |key: &str| role.get(key).and_then(Value::as_str).map(str::to_string);
    Ok(Credentials {
        access_key_id: field("accessKeyId").ok_or("SSO response is missing accessKeyId")?,
        secret_access_key: field("secretAccessKey").ok_or("SSO response is missing secretAccessKey")?,
        session_token: field("sessionToken"),
        expiration: role["expiration"].as_u64().map(|millis| UNIX_EPOCH + Duration::from_millis(millis)),
    })
}
