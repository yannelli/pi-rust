//! Extension slots that read usage with a built-in fetcher, the fields callers need for quota
//! decisions, and error kinds with `retry-after`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::{UsageEndpoints, UsageErrorKind, UsageService, WindowScope};
use crate::auth::{AuthResolver, Credential, InMemoryCredentialStore, OAuthCredential};
use crate::providers::mock_http::{MockHttp, Reply};

fn oauth(access: &str, extra: Value) -> Credential {
    let extra: Map<String, Value> = serde_json::from_value(extra).unwrap();
    Credential::OAuth(OAuthCredential { refresh: "r".into(), access: access.into(), expires: i64::MAX / 2, extra })
}

fn service(base_url: &str, credentials: Vec<(&str, Credential)>) -> UsageService {
    let credentials: BTreeMap<String, Credential> =
        credentials.into_iter().map(|(provider, credential)| (provider.to_string(), credential)).collect();
    let store = Arc::new(InMemoryCredentialStore::new(credentials));
    let resolver = AuthResolver::new(store, Arc::new(crate::auth::oauth::builtin_oauth));
    let url = || base_url.to_string();
    let endpoints = UsageEndpoints { anthropic: url(), chatgpt: url(), cursor: url(), grok: url() };
    UsageService::with_endpoints(Arc::new(resolver), endpoints)
}

const ANTHROPIC: &str = r#"{"plan_type": "max", "five_hour": {"utilization": 42, "resets_at": "2026-10-30T17:00:00.123+00:00"},
  "seven_day": {"used_percentage": 18, "reset_at": "2026-11-04T09:00:00Z"}, "seven_day_opus": {"utilization": 7, "resets_at": null}}"#;

#[tokio::test]
async fn a_slot_reads_with_its_own_login_and_keeps_model_windows() {
    let body: Value = serde_json::from_str(ANTHROPIC).unwrap();
    let server = MockHttp::start(vec![Reply::json(200, &body), Reply::json(200, &body)]).await;
    let credentials = vec![("anthropic", oauth("tok-base", json!({}))), ("anthropic-2", oauth("tok-slot", json!({})))];
    let usage = service(&server.base_url, credentials);
    assert!(usage.set_alias("anthropic-2", Some("anthropic")));
    let slot = usage.get("anthropic-2", false).await;
    assert_eq!(
        (slot.provider.as_str(), slot.plan.as_deref(), slot.error.as_deref()),
        ("anthropic-2", Some("max"), None)
    );
    let ids: Vec<Option<&str>> = slot.windows.iter().map(|window| window.id.as_deref()).collect();
    assert_eq!(ids, [Some("five_hour"), Some("seven_day"), Some("seven_day_opus")]);
    assert_eq!(slot.windows[0].resets_at_ms, Some(1_793_379_600_123));
    assert_eq!((slot.windows[1].used_percent, slot.windows[1].resets_at_ms), (18.0, Some(1_793_782_800_000)));
    assert_eq!((slot.windows[0].scope, slot.windows[2].scope), (WindowScope::Provider, WindowScope::Model));
    assert_eq!(slot.windows[2].families, ["opus"]);
    assert_eq!(server.request(0).headers["authorization"], "Bearer tok-slot");
    usage.get("anthropic", false).await;
    assert_eq!(server.request(1).headers["authorization"], "Bearer tok-base");
    assert_eq!(usage.logged_in().await, ["anthropic", "anthropic-2"]);
    let sent = serde_json::to_value(&slot.windows[0]).unwrap();
    assert_eq!((&sent["scope"], &sent["id"]), (&json!("provider"), &json!("five_hour")));
}

#[tokio::test]
async fn removing_or_moving_an_alias_drops_the_cached_slot() {
    let body: Value = serde_json::from_str(ANTHROPIC).unwrap();
    let server = MockHttp::start(vec![Reply::json(200, &body), Reply::json(200, &body)]).await;
    let usage = service(&server.base_url, vec![("anthropic-2", oauth("tok-slot", json!({})))]);
    usage.set_alias("anthropic-2", Some("anthropic"));
    assert_eq!(usage.get("anthropic-2", false).await.error, None);
    assert!(!usage.set_alias("anthropic-2", None));
    let gone = usage.get("anthropic-2", false).await;
    assert_eq!((gone.error_kind, gone.windows.len()), (Some(UsageErrorKind::Unsupported), 0));
    usage.set_alias("anthropic-2", Some("anthropic"));
    assert_eq!(usage.get("anthropic-2", false).await.error, None);
    assert_eq!(server.requests().len(), 2, "a removed alias kept its cached usage");
    assert!(!usage.set_alias("anthropic", Some("openai-codex")), "a built-in provider became an alias");
    assert!(!usage.set_alias("anthropic-3", Some("anthropic-2")), "an alias pointed at another alias");
    assert!(!usage.set_alias("anthropic-3", Some("nope")));
    assert!(!usage.set_alias("antigravity-2", Some("google-antigravity")), "an ACP slot would share the one session");
    assert_eq!(usage.get("antigravity-2", false).await.error_kind, Some(UsageErrorKind::Unsupported));
    usage.set_alias("anthropic-2", Some("github-copilot"));
    assert_eq!(
        usage.get("anthropic-2", false).await.error_kind,
        Some(UsageErrorKind::Unsupported),
        "a base without a fetcher kept the old alias"
    );
}

#[tokio::test]
async fn a_codex_slot_sends_its_account_and_reads_credits_and_relative_resets() {
    let body = json!({
        "plan_type": "pro",
        "rate_limit": {"primary_window": {"used_percent": 42, "limit_window_seconds": 301, "reset_after_seconds": 600},
            "secondary_window": {"used_percent": 5, "reset_at": 1_793_379_600}},
        "additional_rate_limits": [{"limit_name": "Spark", "rate_limit": {"primary_window": {"used_percent": 1}}}],
        "credits": {"has_credits": true, "unlimited": false, "balance": "12.50"},
    });
    let server = MockHttp::start(vec![Reply::json(200, &body)]).await;
    let usage = service(&server.base_url, vec![("openai-codex-2", oauth("tok-codex", json!({"accountId": "acct-2"})))]);
    usage.set_alias("openai-codex-2", Some("openai-codex"));
    let before = crate::now_ms();
    let slot = usage.get("openai-codex-2", false).await;
    assert_eq!(server.request(0).headers["chatgpt-account-id"], "acct-2");
    let credits = slot.credits.clone().expect("credits");
    assert_eq!((credits.available, credits.unlimited, credits.balance.as_deref()), (true, false, Some("12.50")));
    let ids: Vec<&str> = slot.windows.iter().filter_map(|window| window.id.as_deref()).collect();
    assert_eq!(ids, ["primary_window", "secondary_window", "Spark/primary_window"]);
    let reset = slot.windows[0].resets_at_ms.expect("a relative reset");
    assert!((before + 600_000..=crate::now_ms() + 600_000).contains(&reset), "{reset}");
    assert_eq!((slot.windows[1].resets_at_ms, slot.windows[1].window_minutes), (Some(1_793_379_600_000), None));
    assert_eq!((slot.windows[0].window_seconds, slot.windows[0].window_minutes), (Some(301), Some(5)));
    assert_eq!(slot.windows[1].window_seconds, None);
}

#[tokio::test]
async fn failures_carry_their_kind_and_retry_after() {
    let replies = vec![
        Reply::json(429, &json!({})).header("retry-after", "120"),
        Reply::json(503, &json!({})).header("retry-after", "Wed, 21 Oct 2099 07:28:00 GMT"),
        Reply::json(401, &json!({})),
    ];
    let server = MockHttp::start(replies).await;
    let usage = service(&server.base_url, vec![("anthropic", oauth("tok", json!({})))]);
    let limited = usage.get("anthropic", true).await;
    assert_eq!((limited.error_kind, limited.retry_after_ms), (Some(UsageErrorKind::RateLimited), Some(120_000)));
    let unavailable = usage.get("anthropic", true).await;
    assert_eq!(unavailable.error_kind, Some(UsageErrorKind::Http));
    let until = 4_096_250_880_000 - crate::now_ms();
    assert!(
        unavailable.retry_after_ms.is_some_and(|ms| (ms - until).abs() < 5_000),
        "{:?}",
        unavailable.retry_after_ms
    );
    let rejected = usage.get("anthropic", true).await;
    assert_eq!((rejected.error_kind, rejected.retry_after_ms), (Some(UsageErrorKind::Rejected), None));
    let none = usage.get("openai-codex", false).await;
    assert_eq!(none.error_kind, Some(UsageErrorKind::Login));
    let sent = serde_json::to_value(&limited).unwrap();
    assert_eq!((&sent["errorKind"], &sent["retryAfterMs"]), (&json!("rateLimited"), &json!(120_000)));
}

#[tokio::test]
async fn invalid_window_lengths_have_no_exact_seconds() {
    let body = json!({"rate_limit": {
        "primary_window": {"used_percent": 1, "limit_window_seconds": 0},
        "secondary_window": {"used_percent": 2, "limit_window_seconds": 300.5}}});
    let server = MockHttp::start(vec![Reply::json(200, &body)]).await;
    let usage = service(&server.base_url, vec![("openai-codex", oauth("tok", json!({})))]);
    let windows = usage.get("openai-codex", false).await.windows;
    let lengths: Vec<(Option<u64>, Option<u64>)> =
        windows.iter().map(|w| (w.window_seconds, w.window_minutes)).collect();
    assert_eq!(lengths, [(None, None), (None, Some(5))]);
}

#[tokio::test]
async fn extreme_numbers_give_no_reset_or_length_instead_of_overflowing() {
    let body = json!({"rate_limit": {
        "primary_window": {"used_percent": 1, "limit_window_seconds": 1e308, "reset_after_seconds": 1e308},
        "secondary_window": {"used_percent": 2, "reset_at": 9.3e18}}});
    let server = MockHttp::start(vec![Reply::json(200, &body)]).await;
    let usage = service(&server.base_url, vec![("openai-codex", oauth("tok", json!({})))]);
    let windows = usage.get("openai-codex", false).await.windows;
    assert_eq!((windows[0].window_seconds, windows[0].resets_at_ms, windows[1].resets_at_ms), (None, None, None));
    let now = crate::now_ms();
    for header in ["99999999999999999999", "Wed, 21 Oct 999999999 07:28:00 GMT", "-5", "soon"] {
        assert_eq!(super::retry_after_ms(header, now), None, "{header}");
    }
    assert_eq!(super::time::ms_from_value(&json!("999999999-01-01T00:00:00Z")), None);
    assert_eq!(super::time::ms_from_value(&json!(f64::MAX)), None);
}
