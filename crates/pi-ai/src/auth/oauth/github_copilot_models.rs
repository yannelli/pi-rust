//! GitHub Copilot account catalog, model policy enabling, and rate-limit retries.

use std::time::Duration;

use reqwest::RequestBuilder;
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::http::{Hooks, Reply, SendError, now_ms, send};
use crate::auth::{AuthError, AuthResult};

pub(crate) const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];
const COPILOT_API_VERSION: &str = "2026-06-01";
const INDIVIDUAL_BASE_URL: &str = "https://api.individual.githubcopilot.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The API base URL from a Copilot token's `proxy-ep` field, for example
/// `proxy.individual.githubcopilot.com` becomes `https://api.individual.githubcopilot.com`.
fn base_url_from_token(token: &str) -> Option<String> {
    let start = token.find("proxy-ep=")? + "proxy-ep=".len();
    let proxy_host = token[start..].split(';').next().filter(|host| !host.is_empty())?;
    let api_host =
        proxy_host.strip_prefix("proxy.").map_or_else(|| proxy_host.to_string(), |rest| format!("api.{rest}"));
    Some(format!("https://{api_host}"))
}

/// Base URL for Copilot requests: the token's proxy endpoint, then the enterprise domain.
pub fn github_copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(url) = token.and_then(base_url_from_token) {
        return url;
    }
    match enterprise_domain {
        Some(domain) => format!("https://copilot-api.{domain}"),
        None => INDIVIDUAL_BASE_URL.into(),
    }
}

/// Model ids from the account catalog: those to offer and those whose policy to enable.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CopilotCatalog {
    pub available_model_ids: Vec<String>,
    pub policy_model_ids: Vec<String>,
}

pub(crate) fn parse_catalog(
    raw: &Value,
    allow_policy_fallback: bool,
    known_model: &(dyn Fn(&str) -> bool + Sync),
) -> AuthResult<CopilotCatalog> {
    let data = raw.get("data").and_then(Value::as_array);
    let data = data.ok_or_else(|| AuthError::other("Invalid Copilot models response"))?;
    struct AccountModel<'a> {
        id: &'a str,
        picker_enabled: bool,
        policy_state: Option<&'a str>,
    }
    let account_models: Vec<AccountModel> = data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            let tool_calls =
                item.get("capabilities").and_then(|caps| caps.get("supports")).and_then(|s| s.get("tool_calls"));
            if tool_calls == Some(&Value::Bool(false)) {
                return None;
            }
            let picker_enabled = item.get("model_picker_enabled") == Some(&Value::Bool(true));
            let policy_state = item.get("policy").and_then(|policy| policy.get("state")).and_then(Value::as_str);
            Some(AccountModel { id, picker_enabled, policy_state })
        })
        .collect();
    let ids = |keep: &dyn Fn(&AccountModel) -> bool| -> Vec<String> {
        account_models.iter().filter(|model| keep(model)).map(|model| model.id.to_string()).collect()
    };
    let picker = ids(&|model| model.picker_enabled && model.policy_state != Some("disabled"));
    let use_policy_fallback = allow_policy_fallback && picker.is_empty();
    let policy_model_ids = ids(&|model| {
        model.policy_state == Some("unconfigured")
            && known_model(model.id)
            && (model.picker_enabled || use_policy_fallback)
    });
    let available_model_ids = if !picker.is_empty() || !allow_policy_fallback {
        picker
    } else {
        ids(&|model| model.policy_state == Some("enabled"))
    };
    Ok(CopilotCatalog { available_model_ids, policy_model_ids })
}

#[derive(Clone, Copy)]
pub(crate) struct RetryPolicy {
    pub max_retries: u32,
    pub max_elapsed: Duration,
}

/// JavaScript `parseFloat`: the longest numeric prefix after leading whitespace.
fn parse_float_prefix(text: &str) -> Option<f64> {
    let text = text.trim_start();
    let unsigned = text.trim_start_matches(['+', '-']);
    if unsigned.len() + 1 < text.len() {
        return None;
    }
    if unsigned.starts_with("Infinity") {
        return Some(if text.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY });
    }
    let bytes = text.as_bytes();
    let mut end = text.len() - unsigned.len();
    let digits = |from: usize| bytes[from..].iter().take_while(|byte| byte.is_ascii_digit()).count();
    let integer = digits(end);
    end += integer;
    let mut fraction = 0;
    if bytes.get(end) == Some(&b'.') {
        fraction = digits(end + 1);
        end += 1 + fraction;
    }
    if integer + fraction == 0 {
        return None;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let exponent = digits(end + 1 + sign);
        if exponent > 0 {
            end += 1 + sign + exponent;
        }
    }
    text[..end].trim_end_matches('.').parse().ok()
}

/// Unix milliseconds of an IMF-fixdate such as `Sun, 06 Nov 1994 08:49:37 GMT`.
fn parse_http_date(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = parts.as_slice() else { return None };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS.iter().position(|name| name == month)? as i64 + 1;
    let (day, year): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let clock: Vec<i64> = time.split(':').map(|part| part.parse().ok()).collect::<Option<_>>()?;
    let [hour, minute, second] = clock.as_slice() else { return None };
    // Days from the civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000)
}

/// Sends with a 5 second timeout, retrying 429 responses after `Retry-After` within the budget.
pub(crate) async fn fetch_with_rate_limit_retry(
    hooks: &Hooks,
    build: impl Fn() -> RequestBuilder,
    cancel: &CancellationToken,
    policy: RetryPolicy,
) -> Result<Reply, SendError> {
    let budget = (policy.max_retries > 0 && !policy.max_elapsed.is_zero()).then(|| Instant::now() + policy.max_elapsed);
    let mut retry = 0;
    loop {
        let timeout = budget.map_or(REQUEST_TIMEOUT, |deadline| {
            REQUEST_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()))
        });
        let reply = send(build(), cancel, hooks.timeout(timeout)).await?;
        if reply.status != 429 || retry == policy.max_retries {
            return Ok(reply);
        }
        let mut delay_ms = 500.0 * 2f64.powi(retry as i32);
        if let Some(retry_after) = reply.header("retry-after").filter(|value| !value.is_empty()) {
            delay_ms = match parse_float_prefix(retry_after) {
                Some(seconds) => seconds * 1000.0,
                None => parse_http_date(retry_after).map_or(f64::NAN, |at| (at - now_ms()) as f64),
            };
            if !delay_ms.is_finite() {
                return Ok(reply);
            }
        }
        let delay = Duration::from_millis(delay_ms.max(0.0) as u64);
        if budget.is_some_and(|deadline| delay >= deadline.saturating_duration_since(Instant::now())) {
            return Ok(reply);
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(SendError::Cancelled),
            _ = tokio::time::sleep(delay) => {}
        }
        retry += 1;
    }
}

fn status_error(reply: &Reply) -> AuthError {
    AuthError::other(format!("{} {}: {}", reply.status, reply.status_text(), reply.body))
}

pub(crate) async fn fetch_models(
    hooks: &Hooks,
    copilot_token: &str,
    enterprise_domain: Option<&str>,
    known_model: &(dyn Fn(&str) -> bool + Sync),
    cancel: &CancellationToken,
    policy: RetryPolicy,
) -> AuthResult<CopilotCatalog> {
    let base_url = github_copilot_base_url(Some(copilot_token), enterprise_domain);
    // Some Individual accounts report every picker flag false despite enabled policies.
    let allow_policy_fallback = base_url == INDIVIDUAL_BASE_URL;
    let url = format!("{base_url}/models");
    let build = || {
        COPILOT_HEADERS
            .iter()
            .fold(hooks.get(&url).header("Accept", "application/json"), |request, (name, value)| {
                request.header(*name, *value)
            })
            .header("Authorization", format!("Bearer {copilot_token}"))
            .header("X-GitHub-Api-Version", COPILOT_API_VERSION)
    };
    let reply = fetch_with_rate_limit_retry(hooks, build, cancel, policy).await?;
    if !reply.ok() {
        return Err(status_error(&reply));
    }
    let raw = reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))?;
    parse_catalog(&raw, allow_policy_fallback, known_model)
}

/// Enables one model for the account; `Ok(false)` when the update did not succeed.
async fn enable_model(
    hooks: &Hooks,
    token: &str,
    model_id: &str,
    enterprise_domain: Option<&str>,
    cancel: &CancellationToken,
) -> AuthResult<bool> {
    let url = format!("{}/models/{model_id}/policy", github_copilot_base_url(Some(token), enterprise_domain));
    let build = || {
        COPILOT_HEADERS
            .iter()
            .fold(hooks.post(&url).json(&json!({ "state": "enabled" })), |request, (name, value)| {
                request.header(*name, *value)
            })
            .header("Authorization", format!("Bearer {token}"))
            .header("openai-intent", "chat-policy")
            .header("x-interaction-type", "chat-policy")
    };
    let policy = RetryPolicy { max_retries: 2, max_elapsed: Duration::from_millis(5000) };
    let reply = match fetch_with_rate_limit_retry(hooks, build, cancel, policy).await {
        Ok(reply) => reply,
        Err(SendError::Cancelled) => return Err(AuthError::Cancelled),
        Err(_) => return Ok(false),
    };
    if reply.status == 429 {
        return Err(status_error(&reply));
    }
    Ok(reply.ok())
}

/// Enables the models and returns the ids that succeeded. Best effort: exhausted
/// rate limiting stops the batch.
pub(crate) async fn enable_models(
    hooks: &Hooks,
    token: &str,
    model_ids: &[String],
    enterprise_domain: Option<&str>,
    cancel: &CancellationToken,
) -> AuthResult<Vec<String>> {
    let mut enabled = Vec::new();
    for model_id in model_ids {
        match enable_model(hooks, token, model_id, enterprise_domain, cancel).await {
            Ok(true) => enabled.push(model_id.clone()),
            Ok(false) => {}
            Err(error) if cancel.is_cancelled() => return Err(error),
            Err(_) => break,
        }
    }
    Ok(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_values_parse_like_javascript() {
        assert_eq!(parse_float_prefix("1"), Some(1.0));
        assert_eq!(parse_float_prefix(" 2.5s"), Some(2.5));
        assert_eq!(parse_float_prefix("1e3"), Some(1000.0));
        assert_eq!(parse_float_prefix("3."), Some(3.0));
        assert_eq!(parse_float_prefix("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_float_prefix("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_float_prefix("."), None);
        assert_eq!(parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480_000));
        assert_eq!(parse_http_date("soon"), None);
    }

    #[test]
    fn base_url_comes_from_the_token_then_the_enterprise_domain() {
        let token = "tid=abc;exp=123;proxy-ep=proxy.enterprise.example;rest";
        assert_eq!(github_copilot_base_url(Some(token), None), "https://api.enterprise.example");
        assert_eq!(
            github_copilot_base_url(Some("no-proxy"), Some("company.ghe.com")),
            "https://copilot-api.company.ghe.com"
        );
        assert_eq!(github_copilot_base_url(None, None), INDIVIDUAL_BASE_URL);
    }
}
