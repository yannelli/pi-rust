//! Credential redaction for bug reports (upstream `redactUrl` and `redactJsonValue` in
//! `core/bug-report.ts`).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

pub const REDACTED: &str = "<redacted>";

static SENSITIVE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[-_])(api[-_]?key|secret|token|password|passwd|credential|authorization|cookie)(?:$|[-_])")
        .expect("valid pattern")
});
static NESTED_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)^([a-z][a-z0-9+.-]*:)([a-z][a-z0-9+.-]*://.*)$").expect("valid pattern"));

/// Whether `key` names a secret; camelCase words count (`apiKey`, `refreshToken`).
pub fn is_sensitive_key(key: &str) -> bool {
    let mut snake = String::with_capacity(key.len() + 4);
    let mut previous: Option<char> = None;
    for ch in key.chars() {
        if ch.is_ascii_uppercase() && previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit()) {
            snake.push('_');
        }
        snake.push(ch);
        previous = Some(ch);
    }
    SENSITIVE_KEY.is_match(&snake)
}

/// `value` without URL credentials and with secret-looking query values replaced. Strings that
/// are not URLs, or need no change, come back as they are.
pub fn redact_url(value: &str) -> String {
    if let Some(captures) = NESTED_URL.captures(value) {
        return format!("{}{}", &captures[1], redact_url(&captures[2]));
    }
    let Ok(mut url) = reqwest::Url::parse(value) else { return value.to_string() };
    let mut changed = false;
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        changed = true;
    }
    let pairs: Vec<(String, String)> =
        url.query_pairs().map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
    if pairs.iter().any(|(key, _)| is_sensitive_key(key)) {
        let mut query = url.query_pairs_mut();
        query.clear();
        for (key, value) in &pairs {
            query.append_pair(key, if is_sensitive_key(key) { REDACTED } else { value });
        }
        drop(query);
        changed = true;
    }
    if changed { url.to_string() } else { value.to_string() }
}

/// A copy of `value` with secret-named values replaced and URLs in strings redacted.
pub fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_url(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_json_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| {
                    let redacted = if !child.is_null() && is_sensitive_key(key) {
                        Value::String(REDACTED.into())
                    } else {
                        redact_json_value(child)
                    };
                    (key.clone(), redacted)
                })
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

/// Settings without the analytics and device ids, redacted.
pub fn redact_settings(settings: &Value) -> Value {
    let mut settings = settings.clone();
    if let Some(map) = settings.as_object_mut() {
        map.remove("trackingId");
        map.remove("deviceId");
    }
    redact_json_value(&settings)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn removes_url_credentials_and_secret_query_parameters() {
        assert_eq!(redact_url("https://user:pass@proxy.example.com:8080/"), "https://proxy.example.com:8080/");
        assert_eq!(redact_url("git:https://pat@github.com/org/repo"), "git:https://github.com/org/repo");
        assert_eq!(
            redact_url("https://api.example/v1?api-key=abc&model=x"),
            "https://api.example/v1?api-key=%3Credacted%3E&model=x"
        );
        assert_eq!(redact_url("https://api.example/v1?model=x%20y"), "https://api.example/v1?model=x%20y");
        assert_eq!(redact_url("plain words"), "plain words");
    }

    #[test]
    fn redacts_nested_secret_values_without_hiding_token_counts() {
        let value = json!({
            "apiKey": "sk-123",
            "headers": {"Authorization": "Bearer x", "X-Trace": "1"},
            "compaction": {"reserveTokens": 16384, "keepRecentTokens": 20000},
            "baseUrl": "https://me:secret@example.com/",
            "refresh_token": null,
            "list": ["https://u:p@h/"],
        });
        assert_eq!(
            redact_json_value(&value),
            json!({
                "apiKey": "<redacted>",
                "headers": {"Authorization": "<redacted>", "X-Trace": "1"},
                "compaction": {"reserveTokens": 16384, "keepRecentTokens": 20000},
                "baseUrl": "https://example.com/",
                "refresh_token": null,
                "list": ["https://h/"],
            })
        );
        assert!(is_sensitive_key("refreshToken") && is_sensitive_key("x-api-key") && is_sensitive_key("COOKIE"));
        assert!(!is_sensitive_key("tokens") && !is_sensitive_key("maxTokens"));
        let settings = redact_settings(&json!({"trackingId": "t", "deviceId": "d", "theme": "dark"}));
        assert_eq!(settings, json!({"theme": "dark"}));
    }
}
