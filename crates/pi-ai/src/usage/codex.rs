//! ChatGPT (Codex) plan usage: `GET {chatgpt}/wham/usage` with the account header of the provider.

use serde_json::Value;

use super::time::iso_from_epoch;
use super::{Fetched, UsageWindow, json_response, number};
use crate::providers::openai_codex::chatgpt_account_id;

pub(super) async fn fetch(base_url: &str, token: &str, stored_account: Option<&str>, provider: &str) -> Fetched {
    let mut request = crate::http::shared_client()
        .get(format!("{}/wham/usage", base_url.trim_end_matches('/')))
        .bearer_auth(token)
        .header("accept", "application/json")
        .header("originator", "pi");
    let account = stored_account.map(str::to_string).or_else(|| chatgpt_account_id(token));
    if let Some(account) = account.filter(|account| !account.is_empty()) {
        request = request.header("chatgpt-account-id", account);
    }
    let body = json_response(request, provider).await?;
    let plan = body["plan_type"].as_str().filter(|plan| !plan.is_empty()).map(str::to_string);
    Ok((plan, windows(&body)))
}

fn duration_label(minutes: Option<u64>, fallback: &str) -> String {
    match minutes {
        Some(300) => "5h".into(),
        Some(10_080) => "Weekly".into(),
        Some(minutes) if minutes % 1440 == 0 => format!("{}d", minutes / 1440),
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes}m"),
        None => fallback.into(),
    }
}

fn rate_limit(limit: &Value, prefix: Option<&str>, out: &mut Vec<UsageWindow>) {
    for (slot, fallback) in [("primary_window", "Primary"), ("secondary_window", "Secondary")] {
        let window = &limit[slot];
        let Some(used) = number(&window["used_percent"]) else { continue };
        let minutes = number(&window["limit_window_seconds"]).filter(|seconds| *seconds > 0.0);
        let minutes = minutes.map(|seconds| (seconds / 60.0).round() as u64);
        let label = duration_label(minutes, fallback);
        let label = prefix.map_or_else(|| label.clone(), |prefix| format!("{prefix} {label}"));
        let resets_at = number(&window["reset_at"]).and_then(iso_from_epoch);
        out.push(UsageWindow::new(label, used, resets_at, minutes));
    }
}

/// `rate_limit` windows, then each `additional_rate_limits` entry under its name.
pub(super) fn windows(body: &Value) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    rate_limit(&body["rate_limit"], None, &mut windows);
    for extra in body["additional_rate_limits"].as_array().into_iter().flatten() {
        let name = extra["limit_name"].as_str().or_else(|| extra["metered_feature"].as_str());
        rate_limit(&extra["rate_limit"], Some(name.unwrap_or("Extra")), &mut windows);
    }
    windows
}
