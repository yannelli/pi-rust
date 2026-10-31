use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::{ProviderUsage, UsageEndpoints, UsageService, UsageWindow, anthropic, codex, cursor, grok};
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

/// The windows as [`UsageWindow::new`] builds them, without the id, scope, and exact reset.
fn basic(windows: &[UsageWindow]) -> Vec<UsageWindow> {
    windows
        .iter()
        .map(|w| UsageWindow::new(w.label.clone(), w.used_percent, w.resets_at.clone(), w.window_minutes))
        .collect()
}

fn window(label: &str, used: f64, resets_at: Option<&str>, minutes: Option<u64>) -> UsageWindow {
    UsageWindow::new(label, used, resets_at.map(str::to_string), minutes)
}

#[tokio::test]
async fn anthropic_reads_session_weekly_and_model_windows_with_oauth_headers() {
    let body = json!({
        "five_hour": {"utilization": 42.0, "resets_at": "2026-10-30T17:00:00.000000+00:00"},
        "seven_day": {"utilization": "18", "resets_at": "2026-11-04T09:00:00Z"},
        "seven_day_opus": {"utilization": 7, "resets_at": null},
        "seven_day_oauth_apps": null,
        "limits": [
            {"kind": "weekly_scoped", "percent": 3, "resets_at": "2026-11-04T09:00:00Z",
             "scope": {"model": {"display_name": "Sonnet"}}},
            {"kind": "weekly_scoped", "percent": 9, "scope": {"model": {"display_name": "Opus"}}},
        ],
    });
    let server = MockHttp::start(vec![Reply::json(200, &body)]).await;
    let usage = service(&server.base_url, vec![("anthropic", oauth("sk-ant-oat-1", json!({})))]);
    let result = usage.get("anthropic", false).await;
    assert_eq!(result.error, None);
    assert_eq!(
        basic(&result.windows),
        vec![
            window("5h", 42.0, Some("2026-10-30T17:00:00Z"), Some(300)),
            window("Weekly", 18.0, Some("2026-11-04T09:00:00Z"), Some(10_080)),
            window("Weekly Opus", 7.0, None, Some(10_080)),
            window("Weekly Sonnet", 3.0, Some("2026-11-04T09:00:00Z"), Some(10_080)),
        ]
    );
    let request = server.request(0);
    assert_eq!((request.method.as_str(), request.path.as_str()), ("GET", "/api/oauth/usage"));
    assert_eq!(request.headers["authorization"], "Bearer sk-ant-oat-1");
    assert_eq!(request.headers["anthropic-beta"], "oauth-2025-04-20");
    assert!(request.headers["user-agent"].starts_with("claude-cli/"));
}

#[tokio::test]
async fn results_are_cached_until_a_refresh() {
    let first = json!({"five_hour": {"utilization": 10, "resets_at": null}});
    let second = json!({"five_hour": {"utilization": 20, "resets_at": null}});
    let server = MockHttp::start(vec![Reply::json(200, &first), Reply::json(200, &second)]).await;
    let usage = service(&server.base_url, vec![("anthropic", oauth("sk-ant-oat-1", json!({})))]);
    assert_eq!(usage.get("anthropic", false).await.windows[0].used_percent, 10.0);
    assert_eq!(usage.get("anthropic", false).await.windows[0].used_percent, 10.0);
    assert_eq!(server.requests().len(), 1);
    assert_eq!(usage.get("anthropic", true).await.windows[0].used_percent, 20.0);
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn codex_sends_the_account_and_reads_primary_secondary_and_extra_limits() {
    let body = json!({
        "plan_type": "pro",
        "rate_limit": {
            "primary_window": {"used_percent": 42, "limit_window_seconds": 18000, "reset_at": 1_793_379_600},
            "secondary_window": {"used_percent": 18.5, "limit_window_seconds": 604_800, "reset_at": 1_793_811_600},
        },
        "additional_rate_limits": [{"limit_name": "Spark", "rate_limit": {"primary_window": {"used_percent": 5}}}],
    });
    let server = MockHttp::start(vec![Reply::json(200, &body)]).await;
    let usage = service(&server.base_url, vec![("openai-codex", oauth("tok", json!({"accountId": "acct-1"})))]);
    let result = usage.get("openai-codex", false).await;
    assert_eq!(result.plan.as_deref(), Some("pro"));
    assert_eq!(
        basic(&result.windows),
        vec![
            window("5h", 42.0, Some("2026-10-30T17:00:00Z"), Some(300)),
            window("Weekly", 18.5, Some("2026-11-04T17:00:00Z"), Some(10_080)),
            window("Spark Primary", 5.0, None, None),
        ]
    );
    let request = server.request(0);
    assert_eq!(request.path, "/wham/usage");
    assert_eq!(request.headers["chatgpt-account-id"], "acct-1");
    assert_eq!(request.headers["authorization"], "Bearer tok");
}

#[tokio::test]
async fn cursor_posts_connect_json_and_reads_plan_percentages() {
    let body = json!({
        "planUsage": {"totalPercentUsed": 61.5, "autoPercentUsed": 40, "apiPercentUsed": null},
        "billingCycleStart": "1790787600000",
        "billingCycleEnd": "1793379600000",
    });
    let server = MockHttp::start(vec![Reply::json(200, &body)]).await;
    let usage = service(&server.base_url, vec![("cursor", oauth("cur", json!({})))]);
    let result = usage.get("cursor", false).await;
    let cycle = Some(43_200);
    assert_eq!(
        basic(&result.windows),
        vec![
            window("Total", 61.5, Some("2026-10-30T17:00:00Z"), cycle),
            window("Auto", 40.0, Some("2026-10-30T17:00:00Z"), cycle)
        ]
    );
    let request = server.request(0);
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("POST", "/aiserver.v1.DashboardService/GetCurrentPeriodUsage")
    );
    assert_eq!(request.headers["connect-protocol-version"], "1");
    assert_eq!(request.json(), json!({}));
}

#[tokio::test]
async fn failures_and_missing_logins_return_an_error_state() {
    let server =
        MockHttp::start(vec![Reply::json(401, &json!({"error": "expired"})), Reply::text(200, "text/html", "<html>")])
            .await;
    let usage = service(
        &server.base_url,
        vec![("anthropic", oauth("sk-ant-oat-1", json!({}))), ("cursor", oauth("cur", json!({})))],
    );
    let rejected = usage.get("anthropic", false).await;
    assert_eq!(rejected.error.as_deref(), Some("The login was rejected (HTTP 401). Run /login anthropic."));
    assert!(rejected.windows.is_empty());
    assert_eq!(usage.get("cursor", false).await.error.as_deref(), Some("The usage endpoint returned HTML"));
    let missing = usage.get("openai-codex", false).await;
    assert_eq!(missing.error.as_deref(), Some("Not logged in. Run /login openai-codex."));
    let unknown: ProviderUsage = usage.get("mistral", false).await;
    assert_eq!(unknown.error.as_deref(), Some("pi has no usage report for mistral"));
    assert_eq!(usage.logged_in().await, ["anthropic", "cursor"]);
}

#[test]
fn parsers_skip_absent_windows() {
    assert!(anthropic::windows(&json!({"five_hour": null, "seven_day": {"resets_at": "x"}})).is_empty());
    assert!(codex::windows(&json!({"rate_limit": null}), 0).is_empty());
    assert!(cursor::windows(&json!({})).is_empty());
    assert!(grok::weekly(&json!({"currentPeriod": {"start": "2026-10-01T00:00:00Z", "end": "2026-11-01T00:00:00Z", "type": "MONTH"}}), 0).is_empty());
    assert_eq!(grok::monthly(&json!({"monthlyLimit": {"val": 0}, "used": {"val": 3}})), None);
}

#[tokio::test]
async fn grok_reads_weekly_credits_and_products_with_the_cli_headers() {
    let body = json!({"config": {
        "currentPeriod": {"start": "2026-10-28T17:00:00Z", "end": "2026-11-04T17:00:00.000Z", "type": "BILLING_PERIOD_TYPE_WEEKLY"},
        "creditUsagePercent": 37.5,
        "productUsage": [{"product": "GrokBuild", "usagePercent": 12}, {"product": "Api"}],
    }});
    let server = MockHttp::start(vec![Reply::json(200, &body)]).await;
    let usage = service(&server.base_url, vec![("xai", oauth("grok-token", json!({})))]);
    let result = usage.get("xai", false).await;
    let reset = Some("2026-11-04T17:00:00Z");
    assert_eq!(
        basic(&result.windows),
        vec![
            window("Weekly", 37.5, reset, Some(10_080)),
            window("Weekly Grok Build", 12.0, reset, Some(10_080)),
            window("Weekly API", 0.0, reset, Some(10_080)),
        ]
    );
    let request = server.request(0);
    assert_eq!((request.method.as_str(), request.path.as_str()), ("GET", "/v1/billing?format=credits"));
    assert_eq!(request.headers["authorization"], "Bearer grok-token");
    assert_eq!(request.headers["x-xai-token-auth"], "xai-grok-cli");
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn grok_unified_billing_falls_back_to_the_monthly_quota() {
    let credits = json!({"config": {"isUnifiedBillingUser": true, "monthlyLimit": {"val": 0}}});
    let monthly = json!({"config": {
        "billingPeriodStart": "2026-10-01T00:00:00Z", "billingPeriodEnd": "2026-10-31T00:00:00Z",
        "monthlyLimit": {"val": 2000}, "used": {"val": "500"},
    }});
    let server = MockHttp::start(vec![Reply::json(200, &credits), Reply::json(200, &monthly)]).await;
    let usage = service(&server.base_url, vec![("xai", oauth("grok-token", json!({})))]);
    let result = usage.get("xai", false).await;
    assert_eq!(basic(&result.windows), vec![window("Monthly", 25.0, Some("2026-10-31T00:00:00Z"), Some(43_200))]);
    assert_eq!(server.request(1).path, "/v1/billing");
    let empty = MockHttp::start(vec![Reply::json(200, &json!({"config": {}})), Reply::json(200, &json!({}))]).await;
    let usage = service(&empty.base_url, vec![("xai", oauth("grok-token", json!({})))]);
    assert_eq!(usage.get("xai", false).await.error.as_deref(), Some("The Grok billing response has no usage data"));
}
