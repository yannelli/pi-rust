//! `external_account` (workload and workforce identity federation) and
//! `external_account_authorized_user` credentials, as `google-auth-library` handles them.

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::{CLOUD_PLATFORM_SCOPE, DEFAULT_UNIVERSE, seconds_until, string_field};
use crate::request::error_chain;

const STS_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const STS_REQUEST_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const DEFAULT_TOKEN_LIFESPAN: i64 = 3600;
const NO_IDENTITY_POOL_SOURCE: &str =
    "No valid Identity Pool \"credential_source\" provided, must be either file, url, or certificate.";

/// Rotated `external_account_authorized_user` refresh tokens, keyed by the configured one.
static ROTATED_REFRESH_TOKENS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

/// A response body with its status, read as text and parsed as JSON when possible.
pub(super) struct Reply {
    pub status: u16,
    pub text: String,
    pub json: Value,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The gaxios message for a non-2xx response.
    pub fn failure(&self) -> String {
        format!("Request failed with status code {}", self.status)
    }
}

pub(super) async fn send(request: reqwest::RequestBuilder) -> Result<Reply, String> {
    let response = request.send().await.map_err(|error| error_chain(&error))?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| error_chain(&error))?;
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok(Reply { status, text, json })
}

/// `field` or its camelCase spelling, as `originalOrCamelOptions` reads options.
pub(super) fn option<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    let mut camel = String::new();
    let mut upper = false;
    for character in field.chars() {
        match character {
            '_' => upper = true,
            _ if upper => {
                camel.extend(character.to_uppercase());
                upper = false;
            }
            _ => camel.push(character),
        }
    }
    [field, camel.as_str()].into_iter().map(|key| &value[key]).find(|found| !found.is_null())
}

fn option_str<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    option(value, field).and_then(Value::as_str).filter(|text| !text.is_empty())
}

/// The `Error code ...` message `getErrorFromOAuthErrorResponse` builds from an OAuth error body.
fn oauth_error(reply: &Reply) -> String {
    match reply.json["error"].as_str() {
        Some(code) => {
            let mut message = format!("Error code {code}");
            if let Some(description) = reply.json["error_description"].as_str() {
                message.push_str(&format!(": {description}"));
            }
            if let Some(uri) = reply.json["error_uri"].as_str() {
                message.push_str(&format!(" - {uri}"));
            }
            message
        }
        None => format!("{}: {}", reply.failure(), reply.text.trim()),
    }
}

fn basic_auth(client_id: &str, client_secret: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{client_id}:{client_secret}")))
}

/// True when `audience` names a workforce pool provider.
fn is_workforce_audience(audience: &str) -> bool {
    let marker = "//iam.googleapis.com/locations/";
    audience.match_indices(marker).any(|(start, _)| {
        let rest = &audience[start + marker.len()..];
        let parts: Vec<&str> = rest.splitn(5, '/').collect();
        parts.len() == 5
            && !parts[0].is_empty()
            && parts[1] == "workforcePools"
            && !parts[2].is_empty()
            && parts[3] == "providers"
            && !parts[4].is_empty()
    })
}

/// A federated token: subject token, STS exchange, then optional service account impersonation.
pub(super) async fn external_account_token(
    credentials: &Value,
    env: &BTreeMap<String, String>,
) -> Result<(String, i64), String> {
    let universe = option_str(credentials, "universe_domain").unwrap_or(DEFAULT_UNIVERSE);
    let token_url =
        option_str(credentials, "token_url").map_or_else(|| format!("https://sts.{universe}/v1/token"), str::to_string);
    let audience = option_str(credentials, "audience");
    let subject_token_type = option_str(credentials, "subject_token_type");
    let workforce_project = option_str(credentials, "workforce_pool_user_project");
    if workforce_project.is_some() && !audience.is_some_and(is_workforce_audience) {
        return Err("workforcePoolUserProject should not be set for non-workforce pool credentials.".into());
    }
    let impersonation_url = option_str(credentials, "service_account_impersonation_url");
    let lifetime = option(credentials, "service_account_impersonation")
        .and_then(|impersonation| option(impersonation, "token_lifetime_seconds"))
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds != 0);
    let client_auth = option_str(credentials, "client_id")
        .map(|id| basic_auth(id, option_str(credentials, "client_secret").unwrap_or_default()));

    let source = option(credentials, "credential_source").filter(|source| source.is_object());
    let mut client = crate::http::client_for_env(env);
    let (subject_token, source_type) = match source {
        Some(source) if option(source, "environment_id").is_some() => {
            (super::aws::subject_token(source, audience.unwrap_or_default(), env, &client).await?, "aws")
        }
        Some(source) if option(source, "executable").is_some() => {
            if let Some(url) = impersonation_url.filter(|url| url.len() > 256) {
                return Err(format!("URL is too long: {url}"));
            }
            let context = super::executable::Context {
                audience: audience.unwrap_or_default(),
                subject_token_type: subject_token_type.unwrap_or_default(),
                impersonated_email: impersonation_url.and_then(impersonated_email),
            };
            (super::executable::subject_token(source, &context, env).await?, "executable")
        }
        Some(source) => identity_pool_token(source, env, &mut client).await?,
        None => return Err("A credential source or subject token supplier must be specified.".into()),
    };

    let mut form: Vec<(&str, String)> = vec![("grant_type", STS_GRANT_TYPE.into())];
    if let Some(audience) = audience {
        form.push(("audience", audience.into()));
    }
    form.push(("scope", CLOUD_PLATFORM_SCOPE.into()));
    form.push(("requested_token_type", STS_REQUEST_TOKEN_TYPE.into()));
    form.push(("subject_token", subject_token));
    if let Some(kind) = subject_token_type {
        form.push(("subject_token_type", kind.into()));
    }
    if let (None, Some(project)) = (&client_auth, workforce_project) {
        form.push(("options", json!({"userProject": project}).to_string()));
    }
    let metrics = format!(
        "google-byoid-sdk source/{source_type} sa-impersonation/{} config-lifetime/{}",
        impersonation_url.is_some(),
        lifetime.is_some()
    );
    let mut request = client.post(&token_url).header("x-goog-api-client", metrics).form(&form);
    if let Some(authorization) = &client_auth {
        request = request.header("authorization", authorization);
    }
    let reply = send(request).await.map_err(|error| format!("STS token exchange failed: {error}"))?;
    if !reply.ok() {
        return Err(oauth_error(&reply));
    }
    let access_token =
        string_field(&reply.json, "access_token").ok_or("STS response did not include an access_token")?;
    let Some(url) = impersonation_url else {
        return Ok((access_token.to_string(), reply.json["expires_in"].as_i64().unwrap_or(DEFAULT_TOKEN_LIFESPAN)));
    };
    let body = json!({
        "scope": [CLOUD_PLATFORM_SCOPE],
        "lifetime": format!("{}s", lifetime.unwrap_or(DEFAULT_TOKEN_LIFESPAN)),
    });
    let request = client.post(url).bearer_auth(access_token).json(&body);
    let reply = send(request).await.map_err(|error| format!("Service account impersonation failed: {error}"))?;
    if !reply.ok() {
        let detail = reply.json["error"]["message"].as_str().unwrap_or(reply.text.trim());
        return Err(format!("{}: {detail}", reply.failure()));
    }
    let token =
        string_field(&reply.json, "accessToken").ok_or("Impersonation response did not include an accessToken")?;
    Ok((token.to_string(), seconds_until(reply.json["expireTime"].as_str())))
}

/// The service account email in a `.../serviceAccounts/{email}:generateAccessToken` URL.
fn impersonated_email(url: &str) -> Option<String> {
    let path = url.strip_suffix(":generateAccessToken")?;
    let (_, email) = path.rsplit_once("serviceAccounts/")?;
    (!email.is_empty() && !email.contains(':')).then(|| email.to_string())
}

/// A subject token from an identity pool `file`, `url`, or `certificate` source.
async fn identity_pool_token(
    source: &Value,
    env: &BTreeMap<String, String>,
    client: &mut reqwest::Client,
) -> Result<(String, &'static str), String> {
    let format = option(source, "format").unwrap_or(&Value::Null);
    let format_type = option_str(format, "type").unwrap_or("text");
    let field_name = option_str(format, "subject_token_field_name");
    if format_type != "json" && format_type != "text" {
        return Err(format!("Invalid credential_source format \"{format_type}\""));
    }
    if format_type == "json" && field_name.is_none() {
        return Err("Missing subject_token_field_name for JSON credential_source format".into());
    }
    let file = option_str(source, "file");
    let url = option_str(source, "url");
    let certificate = option(source, "certificate").filter(|value| value.as_bool() != Some(false));
    if [file.is_some(), url.is_some(), certificate.is_some()].iter().filter(|set| **set).count() > 1 {
        return Err(NO_IDENTITY_POOL_SOURCE.into());
    }
    let extract = |text: String, place: &str| -> Result<String, String> {
        let token = match field_name.filter(|_| format_type == "json") {
            Some(name) => {
                let parsed: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
                parsed[name].as_str().map(str::to_string).unwrap_or_default()
            }
            None => text,
        };
        match token.is_empty() {
            true => Err(format!("Unable to parse the subject_token from the credential_source {place}")),
            false => Ok(token),
        }
    };
    if let Some(file) = file {
        let not_file = |detail: String| format!("The file at {file} does not exist, or it is not a file. {detail}");
        let path = tokio::fs::canonicalize(file).await.map_err(|error| not_file(error.to_string()))?;
        let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|error| not_file(error.to_string()))?;
        if !metadata.is_file() {
            return Err(not_file(String::new()));
        }
        let text = tokio::fs::read_to_string(&path).await.map_err(|error| error.to_string())?;
        return Ok((extract(text, "file")?, "file"));
    }
    if let Some(url) = url {
        let mut request = client.get(url);
        if let Some(headers) = option(source, "headers").and_then(Value::as_object) {
            for (name, value) in headers {
                request = request.header(name, value.as_str().map_or_else(|| value.to_string(), str::to_string));
            }
        }
        let reply = send(request).await.map_err(|error| format!("Subject token request failed: {error}"))?;
        if !reply.ok() {
            return Err(reply.failure());
        }
        return Ok((extract(reply.text, "URL")?, "url"));
    }
    if let Some(certificate) = certificate {
        let (token, mtls) = super::certificate::subject_token(certificate, env).await?;
        *client = mtls;
        return Ok((token, "certificate"));
    }
    Err(NO_IDENTITY_POOL_SOURCE.into())
}

/// Refresh an `external_account_authorized_user` (workforce identity gcloud login) token.
pub(super) async fn authorized_user_token(
    credentials: &Value,
    env: &BTreeMap<String, String>,
) -> Result<(String, i64), String> {
    let universe = string_field(credentials, "universe_domain").unwrap_or(DEFAULT_UNIVERSE);
    let token_url = string_field(credentials, "token_url")
        .map_or_else(|| format!("https://sts.{universe}/v1/oauthtoken"), str::to_string);
    let configured = credentials["refresh_token"].as_str().unwrap_or_default().to_string();
    let rotated =
        ROTATED_REFRESH_TOKENS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&configured).cloned();
    let refresh_token = rotated.unwrap_or_else(|| configured.clone());
    let authorization = basic_auth(
        credentials["client_id"].as_str().unwrap_or_default(),
        credentials["client_secret"].as_str().unwrap_or_default(),
    );
    let form = [("grant_type", "refresh_token"), ("refresh_token", refresh_token.as_str())];
    let request = crate::http::client_for_env(env).post(token_url).header("authorization", authorization).form(&form);
    let reply = send(request).await.map_err(|error| format!("Google token request failed: {error}"))?;
    if !reply.ok() {
        return Err(oauth_error(&reply));
    }
    let token = string_field(&reply.json, "access_token").ok_or("Token response did not include an access_token")?;
    if let Some(next) = string_field(&reply.json, "refresh_token") {
        ROTATED_REFRESH_TOKENS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(configured, next.into());
    }
    Ok((token.to_string(), reply.json["expires_in"].as_i64().unwrap_or(DEFAULT_TOKEN_LIFESPAN)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_reads_camel_case_and_workforce_audiences_match() {
        let value = json!({"tokenUrl": "a", "subject_token_type": "b"});
        assert_eq!(option_str(&value, "token_url"), Some("a"));
        assert_eq!(option_str(&value, "subject_token_type"), Some("b"));
        assert!(is_workforce_audience("//iam.googleapis.com/locations/global/workforcePools/pool/providers/p"));
        assert!(!is_workforce_audience("//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/x"));
        let url = "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken";
        assert_eq!(impersonated_email(url).as_deref(), Some("sa@p.iam.gserviceaccount.com"));
    }
}
