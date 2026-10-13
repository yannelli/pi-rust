//! IAM Identity Center credentials: ports of the SDK's `fromSSO` credential provider and the
//! `fromSso` token provider, which refreshes `sso-session` tokens through SSO OIDC `CreateToken`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ring::digest;
use serde_json::{Value, json};

use crate::providers::aws::endpoint::{configured_endpoint, service_host};
use crate::providers::aws::profile::{Section, SharedConfig};
use crate::providers::aws::remote;
use crate::providers::aws::sigv4::{Credentials, format_iso8601, hex, parse_rfc3339};
use crate::providers::provider_env::home_dir;
use crate::registry::http_client;

/// Tokens closer than this to expiry are refreshed.
const EXPIRE_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Each SSO session refreshes at most once in this window.
const REFRESH_THROTTLE: Duration = Duration::from_secs(30);
const OIDC_TIMEOUT: Duration = Duration::from_secs(30);

pub const REFRESH_MESSAGE: &str = "To refresh this SSO session run 'aws sso login' with the corresponding profile.";
const LEGACY_REFRESH_MESSAGE: &str = "To refresh this SSO session run aws sso login with the corresponding profile.";

static LAST_REFRESH_TIMES: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Mutex::default);

/// Port of `fromSSO` for a shared config profile. `sso-session` profiles refresh their token;
/// legacy profiles with only `sso_start_url` use the cached token as is.
pub async fn sso_credentials(
    name: &str,
    data: &Section,
    shared: &SharedConfig,
    env: &BTreeMap<String, String>,
) -> Result<Credentials, String> {
    let mut profile = data.clone();
    if let Some(session_name) = data.get("sso_session") {
        let Some(session) = shared.sso_sessions.get(session_name) else {
            return Err(format!("Sso session '{session_name}' could not be found in shared credentials file."));
        };
        for key in ["sso_region", "sso_start_url"] {
            match session.get(key) {
                Some(value) => profile.insert(key.to_string(), value.clone()),
                None => profile.remove(key),
            };
        }
    }
    validate_profile(&profile)?;
    let region = &profile["sso_region"];
    let (access_token, expires_at) = match profile.get("sso_session") {
        Some(session) => session_token(name, session, region, env).await?,
        None => legacy_token(&profile["sso_start_url"], env)?,
    };
    if expires_at <= SystemTime::now() {
        return Err(format!("The SSO session associated with this profile has expired. {LEGACY_REFRESH_MESSAGE}"));
    }
    remote::sso_role_credentials(region, &profile["sso_account_id"], &profile["sso_role_name"], &access_token, env)
        .await
}

fn validate_profile(profile: &Section) -> Result<(), String> {
    let required = ["sso_start_url", "sso_account_id", "sso_region", "sso_role_name"];
    if required.iter().all(|key| profile.get(*key).is_some_and(|value| !value.is_empty())) {
        return Ok(());
    }
    let mut keys: Vec<&str> = profile.keys().map(String::as_str).collect();
    keys.sort_unstable();
    Err(format!(
        "Profile is configured with invalid SSO credentials. Required parameters \"sso_account_id\", \"sso_region\", \"sso_role_name\", \"sso_start_url\". Got {}\nReference: https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sso.html",
        keys.join(", ")
    ))
}

/// Port of `getSSOTokenFilepath`: `~/.aws/sso/cache/<sha1(id)>.json`.
pub fn token_filepath(id: &str, env: &BTreeMap<String, String>) -> PathBuf {
    let name = hex(digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, id.as_bytes()).as_ref());
    home_dir(env).unwrap_or_default().join(".aws").join("sso").join("cache").join(format!("{name}.json"))
}

fn read_token(id: &str, env: &BTreeMap<String, String>) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(token_filepath(id, env)).ok()?).ok()
}

fn expiry(token: &Value) -> SystemTime {
    token["expiresAt"].as_str().and_then(parse_rfc3339).unwrap_or(UNIX_EPOCH)
}

fn legacy_token(start_url: &str, env: &BTreeMap<String, String>) -> Result<(String, SystemTime), String> {
    let token = read_token(start_url, env)
        .ok_or_else(|| format!("The SSO session associated with this profile is invalid. {LEGACY_REFRESH_MESSAGE}"))?;
    let access_token = token["accessToken"].as_str().unwrap_or_default().to_string();
    Ok((access_token, expiry(&token)))
}

fn token_key<'a>(token: &'a Value, key: &str, for_refresh: bool) -> Result<&'a str, String> {
    token[key].as_str().ok_or_else(|| {
        let suffix = if for_refresh { ". Cannot refresh" } else { "" };
        format!("Value not present for '{key}' in SSO Token{suffix}. {REFRESH_MESSAGE}")
    })
}

fn validate_expiry(expires_at: SystemTime) -> Result<(), String> {
    if expires_at < SystemTime::now() {
        return Err(format!("Token is expired. {REFRESH_MESSAGE}"));
    }
    Ok(())
}

/// Port of the `fromSso` token provider for the session named in a profile.
async fn session_token(
    profile: &str,
    session: &str,
    region: &str,
    env: &BTreeMap<String, String>,
) -> Result<(String, SystemTime), String> {
    let mut token = read_token(session, env).ok_or_else(|| {
        format!(
            "The SSO session token associated with profile={profile} was not found or is invalid. {REFRESH_MESSAGE}"
        )
    })?;
    let access_token = token_key(&token, "accessToken", false)?.to_string();
    token_key(&token, "expiresAt", false)?;
    let expires_at = expiry(&token);
    if expires_at.duration_since(SystemTime::now()).is_ok_and(|left| left > EXPIRE_WINDOW) {
        return Ok((access_token, expires_at));
    }
    let throttled = LAST_REFRESH_TIMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session)
        .is_some_and(|last| last.elapsed() < REFRESH_THROTTLE);
    if throttled {
        validate_expiry(expires_at)?;
        return Ok((access_token, expires_at));
    }
    let client_id = token_key(&token, "clientId", true)?.to_string();
    let client_secret = token_key(&token, "clientSecret", true)?.to_string();
    let refresh_token = token_key(&token, "refreshToken", true)?.to_string();
    LAST_REFRESH_TIMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session.to_string(), Instant::now());
    let body = json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "grantType": "refresh_token",
        "refreshToken": refresh_token,
    });
    let Ok(refreshed) = create_token(region, &body, env).await else {
        validate_expiry(expires_at)?;
        return Ok((access_token, expires_at));
    };
    if let Some(fields) = token.as_object_mut() {
        fields.insert("accessToken".into(), refreshed.access_token.clone().into());
        fields.insert("expiresAt".into(), format_iso8601(refreshed.expires_at).into());
        match refreshed.refresh_token {
            Some(next) => fields.insert("refreshToken".into(), next.into()),
            None => fields.shift_remove("refreshToken"),
        };
        if let Ok(text) = serde_json::to_string_pretty(&token) {
            let _ = std::fs::write(token_filepath(session, env), text);
        }
    }
    Ok((refreshed.access_token, refreshed.expires_at))
}

struct RefreshedToken {
    access_token: String,
    expires_at: SystemTime,
    refresh_token: Option<String>,
}

/// SSO OIDC `CreateToken` with a `refresh_token` grant. The operation is unsigned.
async fn create_token(region: &str, body: &Value, env: &BTreeMap<String, String>) -> Result<RefreshedToken, String> {
    let base =
        configured_endpoint("SSO OIDC", env).unwrap_or_else(|| format!("https://{}", service_host("oidc", region)));
    let builder = http_client()
        .post(format!("{}/token", base.trim_end_matches('/')))
        .timeout(OIDC_TIMEOUT)
        .header("content-type", "application/json")
        .body(body.to_string());
    let text = remote::send_text(builder).await?;
    let value: Value = serde_json::from_str(&text).map_err(|error| format!("Invalid CreateToken response: {error}"))?;
    let access_token = token_key(&value, "accessToken", false)?.to_string();
    let expires_in = value["expiresIn"]
        .as_u64()
        .ok_or_else(|| format!("Value not present for 'expiresIn' in SSO Token. {REFRESH_MESSAGE}"))?;
    Ok(RefreshedToken {
        access_token,
        expires_at: SystemTime::now() + Duration::from_secs(expires_in),
        refresh_token: value["refreshToken"].as_str().map(str::to_string),
    })
}
