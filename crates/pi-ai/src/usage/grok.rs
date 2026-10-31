//! SuperGrok (the `xai` login) usage from the Grok CLI billing endpoint: weekly credits from
//! `?format=credits`, else the monthly included quota of unified billing.

use serde_json::Value;

use super::time::{normalize, unix_from_iso};
use super::{Fetched, UsageWindow, json_response, number};

const WEEK: u64 = 10_080;

async fn billing(base_url: &str, token: &str, query: &str) -> Result<Value, super::UsageError> {
    let request = crate::http::shared_client()
        .get(format!("{}/v1/billing{query}", base_url.trim_end_matches('/')))
        .bearer_auth(token)
        .header("accept", "application/json")
        .header("x-xai-token-auth", "xai-grok-cli");
    json_response(request, "xai").await
}

pub(super) async fn fetch(base_url: &str, token: &str) -> Fetched {
    let credits = billing(base_url, token, "?format=credits").await?;
    let mut windows = weekly(&credits["config"], crate::now_ms() / 1000);
    if windows.is_empty() || credits["config"]["isUnifiedBillingUser"] == true {
        let monthly_config = match monthly(&credits["config"]) {
            Some(window) => Some(window),
            None => monthly(&billing(base_url, token, "").await?["config"]),
        };
        windows.extend(monthly_config);
    }
    if windows.is_empty() {
        return Err("The Grok billing response has no usage data".into());
    }
    Ok((None, windows))
}

fn product_name(product: &str) -> &str {
    match product {
        "GrokBuild" => "Grok Build",
        "Api" => "API",
        other => other,
    }
}

/// `creditUsagePercent` and `productUsage` of a weekly `currentPeriod`; `now` is Unix seconds.
pub(super) fn weekly(config: &Value, now: i64) -> Vec<UsageWindow> {
    let period = &config["currentPeriod"];
    let (Some(start), Some(end)) =
        (period["start"].as_str().and_then(unix_from_iso), period["end"].as_str().and_then(unix_from_iso))
    else {
        return Vec::new();
    };
    let is_week = period["type"].as_str().is_some_and(|kind| kind.to_uppercase().contains("WEEK"));
    if end <= start || !is_week {
        return Vec::new();
    }
    let percent = match number(&config["creditUsagePercent"]) {
        Some(percent) => percent,
        None if config["creditUsagePercent"].is_null() && end > now => 0.0,
        None => return Vec::new(),
    };
    let resets_at = period["end"].as_str().and_then(normalize);
    let mut windows = vec![UsageWindow::new("Weekly", percent, resets_at.clone(), Some(WEEK))];
    for item in config["productUsage"].as_array().into_iter().flatten() {
        let Some(product) = item["product"].as_str().map(str::trim).filter(|product| !product.is_empty()) else {
            continue;
        };
        let used = number(&item["usagePercent"]).unwrap_or(0.0);
        windows.push(UsageWindow::new(
            format!("Weekly {}", product_name(product)),
            used,
            resets_at.clone(),
            Some(WEEK),
        ));
    }
    windows
}

/// `used.val` of `monthlyLimit.val` over `billingPeriodStart` to `billingPeriodEnd`.
pub(super) fn monthly(config: &Value) -> Option<UsageWindow> {
    let start = config["billingPeriodStart"].as_str().and_then(unix_from_iso)?;
    let end_text = config["billingPeriodEnd"].as_str()?;
    let end = unix_from_iso(end_text).filter(|end| *end > start)?;
    let limit = number(&config["monthlyLimit"]["val"]).filter(|limit| *limit > 0.0)?;
    let used = number(&config["used"]["val"])?;
    let days = ((end - start) as f64 / 86_400.0).round().max(1.0) as u64;
    let label = if matches!(days, 30 | 31) { "Monthly".to_string() } else { format!("{days}d") };
    Some(UsageWindow::new(label, used / limit * 100.0, normalize(end_text), Some(((end - start) / 60) as u64)))
}
