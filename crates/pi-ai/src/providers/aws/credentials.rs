//! The AWS default credential chain as the SDK resolves it for Bedrock: environment keys,
//! shared config profiles (static, assumed role, web identity, process, SSO, login),
//! the web identity token file, container credentials, and EC2 instance metadata.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::digest;
use serde_json::Value;

use super::profile::{Section, SharedConfig};
use super::remote::{self, RoleRequest};
use super::sigv4::{Credentials, hex, parse_rfc3339};
use crate::providers::provider_env::{home_dir, provider_env_value};

/// Temporary credentials are refreshed this long before they expire.
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

static CACHE: LazyLock<Mutex<HashMap<String, Credentials>>> = LazyLock::new(Mutex::default);

/// Inputs to the chain. `profile` is a profile chosen for this request, which wins over
/// ambient environment keys; `region` is used for STS when a profile names none.
pub struct ChainInput<'a> {
    pub env: &'a BTreeMap<String, String>,
    pub profile: Option<String>,
    pub region: String,
}

/// Static keys from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and `AWS_SESSION_TOKEN`.
pub fn environment_credentials(env: &BTreeMap<String, String>) -> Option<Credentials> {
    Some(Credentials {
        access_key_id: provider_env_value("AWS_ACCESS_KEY_ID", env)?,
        secret_access_key: provider_env_value("AWS_SECRET_ACCESS_KEY", env)?,
        session_token: provider_env_value("AWS_SESSION_TOKEN", env),
        expiration: provider_env_value("AWS_CREDENTIAL_EXPIRATION", env).and_then(|text| parse_rfc3339(&text)),
    })
}

/// The `region` setting of a shared config profile.
pub fn profile_region(name: &str, env: &BTreeMap<String, String>) -> Option<String> {
    SharedConfig::load(env).profile(name)?.get("region").cloned()
}

/// Resolve credentials. Without a request profile, environment keys win over the ambient profile.
pub async fn resolve(input: &ChainInput<'_>) -> Result<Credentials, String> {
    if input.profile.is_none()
        && let Some(credentials) = environment_credentials(input.env)
    {
        return Ok(credentials);
    }
    let profile = input
        .profile
        .clone()
        .or_else(|| provider_env_value("AWS_PROFILE", input.env))
        .unwrap_or_else(|| "default".into());
    let mut errors = Vec::new();
    let shared = SharedConfig::load(input.env);
    if shared.profile(&profile).is_some() {
        let mut visited = HashSet::new();
        match resolve_profile(&profile, &shared, input, &mut visited, false).await {
            Ok(credentials) => return Ok(credentials),
            Err(error) => errors.push(error),
        }
    } else {
        errors.push(format!("Profile {profile} was not found in the shared AWS config files"));
    }
    if let Some(token_file) = provider_env_value("AWS_WEB_IDENTITY_TOKEN_FILE", input.env) {
        let role_arn = provider_env_value("AWS_ROLE_ARN", input.env)
            .ok_or("AWS_ROLE_ARN is required with AWS_WEB_IDENTITY_TOKEN_FILE")?;
        let session = provider_env_value("AWS_ROLE_SESSION_NAME", input.env);
        return cached(
            &format!("web-identity:{role_arn}"),
            web_identity(&token_file, &role_arn, session, &input.region, input.env),
        )
        .await;
    }
    let container = ["AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "AWS_CONTAINER_CREDENTIALS_FULL_URI"];
    if container.iter().any(|name| provider_env_value(name, input.env).is_some()) {
        return cached(&source_key("container", input.env), remote::container_credentials(input.env)).await;
    }
    match cached(&source_key("imds", input.env), remote::instance_metadata_credentials(input.env)).await {
        Ok(credentials) => Ok(credentials),
        Err(error) => {
            errors.push(error);
            Err(format!("Could not load AWS credentials from any provider: {}", errors.join("; ")))
        }
    }
}

async fn cached<F>(key: &str, load: F) -> Result<Credentials, String>
where
    F: Future<Output = Result<Credentials, String>>,
{
    let hit = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(key).cloned();
    if let Some(credentials) = hit.filter(|credentials| !credentials.expires_within(REFRESH_MARGIN)) {
        return Ok(credentials);
    }
    let credentials = load.await?;
    if credentials.expiration.is_some() {
        CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(key.to_string(), credentials.clone());
    }
    Ok(credentials)
}

/// Cache key for a metadata source, including its configured endpoint.
fn source_key(kind: &str, env: &BTreeMap<String, String>) -> String {
    let names: &[&str] = match kind {
        "imds" => &["AWS_EC2_METADATA_SERVICE_ENDPOINT"],
        _ => &["AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "AWS_CONTAINER_CREDENTIALS_FULL_URI"],
    };
    let endpoint: Vec<String> = names.iter().filter_map(|name| provider_env_value(name, env)).collect();
    format!("{kind}:{}", endpoint.join(","))
}

fn now_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

async fn web_identity(
    token_file: &str,
    role_arn: &str,
    session: Option<String>,
    region: &str,
    env: &BTreeMap<String, String>,
) -> Result<Credentials, String> {
    let token = std::fs::read_to_string(token_file)
        .map_err(|error| format!("Cannot read web identity token file {token_file}: {error}"))?;
    let session_name = session.unwrap_or_else(|| format!("aws-sdk-js-session-{}", now_millis()));
    let request = RoleRequest { role_arn, session_name, region, external_id: None, duration_seconds: None };
    remote::assume_role_with_web_identity(&request, token.trim(), env).await
}

fn static_credentials(section: &Section) -> Option<Credentials> {
    Some(Credentials {
        access_key_id: section.get("aws_access_key_id")?.clone(),
        secret_access_key: section.get("aws_secret_access_key")?.clone(),
        session_token: section.get("aws_session_token").cloned(),
        expiration: None,
    })
}

/// Port of the SDK's `resolveProfileData`; `visited` guards `source_profile` cycles.
fn resolve_profile<'a>(
    name: &'a str,
    shared: &'a SharedConfig,
    input: &'a ChainInput<'a>,
    visited: &'a mut HashSet<String>,
    as_source: bool,
) -> std::pin::Pin<Box<dyn Future<Output = Result<Credentials, String>> + Send + 'a>> {
    Box::pin(async move {
        let empty = Section::new();
        let data = shared.profile(name).unwrap_or(&empty);
        if !visited.is_empty()
            && let Some(credentials) = static_credentials(data)
        {
            return Ok(credentials);
        }
        let has = |key: &str| data.contains_key(key);
        let source_only = !has("role_arn") && has("credential_source");
        let assume_role = has("role_arn") && (has("source_profile") != has("credential_source"));
        if as_source && source_only || assume_role {
            let source = match data.get("source_profile") {
                Some(source) if visited.contains(source) => {
                    let path = visited.iter().cloned().collect::<Vec<_>>().join(", ");
                    return Err(format!(
                        "Detected a cycle attempting to resolve credentials for profile {name}. Profiles visited: {path}"
                    ));
                }
                Some(source) => {
                    visited.insert(source.clone());
                    let source_data = shared.profile(source).unwrap_or(&empty);
                    let nested_source_only =
                        !source_data.contains_key("role_arn") && source_data.contains_key("credential_source");
                    resolve_profile(source, shared, input, visited, nested_source_only).await?
                }
                None => credential_source(data.get("credential_source").map_or("", String::as_str), input.env).await?,
            };
            if source_only {
                return Ok(source);
            }
            if has("mfa_serial") {
                return Err(format!(
                    "Profile {name} requires multi-factor authentication, which pi cannot prompt for."
                ));
            }
            let region = data.get("region").map_or(input.region.as_str(), String::as_str);
            let request = RoleRequest {
                role_arn: &data["role_arn"],
                session_name: data
                    .get("role_session_name")
                    .cloned()
                    .unwrap_or_else(|| format!("aws-sdk-js-{}", now_millis())),
                region,
                external_id: data.get("external_id").map(String::as_str),
                duration_seconds: data.get("duration_seconds").map(String::as_str),
            };
            let key = format!("assume-role:{name}:{}", request.role_arn);
            return cached(&key, remote::assume_role(&request, &source, input.env)).await;
        }
        if let Some(credentials) = static_credentials(data) {
            return Ok(credentials);
        }
        if let (Some(token_file), Some(role_arn)) = (data.get("web_identity_token_file"), data.get("role_arn")) {
            let session = data.get("role_session_name").cloned();
            let region = data.get("region").map_or(input.region.as_str(), String::as_str);
            return cached(
                &format!("web-identity:{role_arn}"),
                web_identity(token_file, role_arn, session, region, input.env),
            )
            .await;
        }
        if let Some(command) = data.get("credential_process") {
            return process_credentials(name, command).await;
        }
        if ["sso_start_url", "sso_account_id", "sso_session", "sso_region", "sso_role_name"].iter().any(|key| has(key))
        {
            return cached(&format!("sso:{name}"), sso_credentials(name, data, shared, input.env)).await;
        }
        if let Some(session) = data.get("login_session") {
            return login_credentials(session, input.env);
        }
        Err(format!("Could not resolve credentials using profile: [{name}] in configuration/credentials file(s)."))
    })
}

async fn credential_source(source: &str, env: &BTreeMap<String, String>) -> Result<Credentials, String> {
    match source {
        "Environment" => {
            environment_credentials(env).ok_or_else(|| "credential_source Environment has no AWS_ACCESS_KEY_ID".into())
        }
        "Ec2InstanceMetadata" => cached(&source_key("imds", env), remote::instance_metadata_credentials(env)).await,
        "EcsContainer" => cached(&source_key("container", env), remote::container_credentials(env)).await,
        other => Err(format!("Unsupported credential source in profile: {other}")),
    }
}

async fn process_credentials(profile: &str, command: &str) -> Result<Credentials, String> {
    #[cfg(windows)]
    let mut process = {
        let mut process = tokio::process::Command::new("cmd");
        process.arg("/C").arg(command);
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = tokio::process::Command::new("/bin/sh");
        process.arg("-c").arg(command);
        process
    };
    let output = process.output().await.map_err(|error| format!("credential_process for {profile} failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("credential_process for {profile} failed: {}", stderr.trim()));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Profile {profile} credential_process returned invalid JSON: {error}"))?;
    if value["Version"] != 1 {
        return Err(format!("Profile {profile} credential_process did not return Version 1."));
    }
    let credentials = remote::credentials_from_json(&value)?;
    if credentials.expires_within(Duration::ZERO) {
        return Err(format!("Profile {profile} credential_process returned expired credentials."));
    }
    Ok(credentials)
}

fn read_json(path: &std::path::Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("Invalid JSON in {}: {error}", path.display()))
}

async fn sso_credentials(
    name: &str,
    data: &Section,
    shared: &SharedConfig,
    env: &BTreeMap<String, String>,
) -> Result<Credentials, String> {
    let session = data.get("sso_session").map(|session| (session, shared.sso_sessions.get(session)));
    let (cache_key, region) = match session {
        Some((session, Some(section))) => (session.clone(), section.get("sso_region")),
        Some((session, None)) => return Err(format!("Profile {name} refers to missing sso-session {session}")),
        None => (data.get("sso_start_url").cloned().unwrap_or_default(), data.get("sso_region")),
    };
    let (Some(region), Some(account), Some(role)) = (region, data.get("sso_account_id"), data.get("sso_role_name"))
    else {
        return Err(format!("Profile {name} is missing sso_region, sso_account_id, or sso_role_name"));
    };
    let digest = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, cache_key.as_bytes());
    let path = home_dir(env).unwrap_or_default().join(".aws/sso/cache").join(format!("{}.json", hex(digest.as_ref())));
    let token = read_json(&path).map_err(|error| format!("{error}. Run `aws sso login --profile {name}`."))?;
    let expired =
        token["expiresAt"].as_str().and_then(parse_rfc3339).is_none_or(|expires| expires <= SystemTime::now());
    let (Some(access_token), false) = (token["accessToken"].as_str(), expired) else {
        return Err(format!("The SSO session for profile {name} has expired. Run `aws sso login --profile {name}`."));
    };
    remote::sso_role_credentials(region, account, role, access_token).await
}

fn login_credentials(session: &str, env: &BTreeMap<String, String>) -> Result<Credentials, String> {
    let directory = provider_env_value("AWS_LOGIN_CACHE_DIRECTORY", env)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home_dir(env).unwrap_or_default().join(".aws/login/cache"));
    let name = hex(digest::digest(&digest::SHA256, session.trim().as_bytes()).as_ref());
    let token = read_json(&directory.join(format!("{name}.json")))?;
    let access = &token["accessToken"];
    let credentials = Credentials {
        access_key_id: access["accessKeyId"].as_str().ok_or("Login cache is missing accessKeyId")?.into(),
        secret_access_key: access["secretAccessKey"].as_str().ok_or("Login cache is missing secretAccessKey")?.into(),
        session_token: access["sessionToken"].as_str().map(str::to_string),
        expiration: access["expiresAt"].as_str().and_then(parse_rfc3339),
    };
    if credentials.expires_within(Duration::ZERO) {
        return Err("AWS login credentials have expired. Run `aws login` again.".into());
    }
    Ok(credentials)
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
