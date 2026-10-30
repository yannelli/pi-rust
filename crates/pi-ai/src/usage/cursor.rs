//! Cursor plan usage: `POST DashboardService/GetCurrentPeriodUsage` (Connect JSON) on the provider host.

use serde_json::{Value, json};

use super::time::iso_from_epoch;
use super::{Fetched, UsageWindow, json_response, number};
use crate::providers::cursor::CURSOR_CLIENT_VERSION;

pub(super) async fn fetch(base_url: &str, token: &str) -> Fetched {
    let request = crate::http::shared_client()
        .post(format!("{}/aiserver.v1.DashboardService/GetCurrentPeriodUsage", base_url.trim_end_matches('/')))
        .bearer_auth(token)
        .header("connect-protocol-version", "1")
        .header("x-cursor-client-version", CURSOR_CLIENT_VERSION)
        .json(&json!({}));
    let body = json_response(request, "cursor").await?;
    Ok((None, windows(&body)))
}

/// The `planUsage` percentages over the billing cycle.
pub(super) fn windows(body: &Value) -> Vec<UsageWindow> {
    let epoch = |key: &str| number(&body[key]);
    let (start, end) = (epoch("billingCycleStart"), epoch("billingCycleEnd"));
    let resets_at = end.and_then(iso_from_epoch);
    let seconds = |value: f64| if value.abs() < 1e10 { value } else { value / 1000.0 };
    let minutes = match (start, end) {
        (Some(start), Some(end)) if end > start => Some(((seconds(end) - seconds(start)) / 60.0).round() as u64),
        _ => None,
    };
    let plan = &body["planUsage"];
    [("totalPercentUsed", "Total"), ("autoPercentUsed", "Auto"), ("apiPercentUsed", "API")]
        .iter()
        .filter_map(|(key, label)| Some(UsageWindow::new(*label, number(&plan[key])?, resets_at.clone(), minutes)))
        .collect()
}
