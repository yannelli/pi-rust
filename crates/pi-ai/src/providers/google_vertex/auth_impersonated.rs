//! `impersonated_service_account` credentials: a source credential's token buys a target
//! service account token from the IAM credentials `generateAccessToken` method.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::external::send;
use super::{CLOUD_PLATFORM_SCOPE, DEFAULT_UNIVERSE, credentials_token, seconds_until, string_field};

const TYPE: &str = "impersonated_service_account";

/// The service account in a `.../{account}:generateAccessToken` (or `generateIdToken`) URL.
fn target_account(url: &str) -> Option<&str> {
    let path = url.strip_suffix(":generateAccessToken").or_else(|| url.strip_suffix(":generateIdToken"))?;
    let account = path.rsplit('/').next()?;
    (!account.is_empty()).then_some(account)
}

pub(super) async fn token(credentials: &Value, env: &BTreeMap<String, String>) -> Result<(String, i64), String> {
    let source_credentials = &credentials["source_credentials"];
    if !source_credentials.is_object() {
        return Err("The incoming JSON object does not contain a source_credentials field".into());
    }
    let Some(url) = string_field(credentials, "service_account_impersonation_url") else {
        return Err("The incoming JSON object does not contain a service_account_impersonation_url field".into());
    };
    if credentials["type"].as_str() != Some(TYPE) {
        return Err(format!("The incoming JSON object does not have the \"{TYPE}\" type"));
    }
    if url.len() > 256 {
        return Err(format!("Target principal is too long: {url}"));
    }
    let account = target_account(url).ok_or_else(|| format!("Cannot extract target principal from {url}"))?;
    let source_universe = string_field(source_credentials, "universe_domain").unwrap_or(DEFAULT_UNIVERSE);
    let universe = match string_field(credentials, "universe_domain") {
        Some(universe) if universe != source_universe => {
            return Err(format!(
                "Universe domain {source_universe} in source credentials does not match {universe} universe domain set for impersonated credentials."
            ));
        }
        Some(universe) => universe,
        None => source_universe,
    };
    let endpoint = string_field(credentials, "endpoint")
        .map_or_else(|| format!("https://iamcredentials.{universe}"), |endpoint| endpoint.to_string());
    let lifetime = match &credentials["lifetime"] {
        Value::Number(number) => number.to_string(),
        _ => "3600".into(),
    };
    let delegates = match &credentials["delegates"] {
        Value::Null => json!([]),
        delegates => delegates.clone(),
    };

    let (source, _) =
        credentials_token(source_credentials, env).await.map_err(|error| format!("unable to impersonate: {error}"))?;
    let target = format!("{endpoint}/v1/projects/-/serviceAccounts/{account}:generateAccessToken");
    let body = json!({"delegates": delegates, "scope": [CLOUD_PLATFORM_SCOPE], "lifetime": format!("{lifetime}s")});
    let mut request = crate::http::client_for_env(env).post(target).bearer_auth(&source.token).json(&body);
    if let Some(quota) = &source.quota_project {
        request = request.header("x-goog-user-project", quota);
    }
    let reply = send(request).await.map_err(|error| format!("unable to impersonate: {error}"))?;
    if !reply.ok() {
        let error = &reply.json["error"];
        return Err(match (error["status"].as_str(), error["message"].as_str()) {
            (Some(status), Some(message)) => format!("{status}: unable to impersonate: {message}"),
            _ => format!("unable to impersonate: {}: {}", reply.failure(), reply.text.trim()),
        });
    }
    let access_token = string_field(&reply.json, "accessToken")
        .ok_or("unable to impersonate: the response did not include an accessToken")?;
    Ok((access_token.to_string(), seconds_until(reply.json["expireTime"].as_str())))
}

#[cfg(test)]
mod tests {
    use super::target_account;

    #[test]
    fn target_accounts_come_from_the_impersonation_url() {
        let url = "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/sa@p.iam.gserviceaccount.com:generateAccessToken";
        assert_eq!(target_account(url), Some("sa@p.iam.gserviceaccount.com"));
        assert_eq!(target_account("https://x/sa:generateIdToken"), Some("sa"));
        assert_eq!(target_account("https://x/sa"), None);
    }
}
