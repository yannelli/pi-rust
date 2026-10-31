//! ChatGPT (Codex) plan usage: `GET {chatgpt}/wham/usage` with the account header of the provider.

use serde_json::Value;

use super::time::{iso_from_unix, ms_from_f64};
use super::{UsageCredits, UsageError, UsageWindow, json_response, number};
use crate::providers::openai_codex::chatgpt_account_id;

pub(super) type CodexFetched = Result<(Option<String>, Vec<UsageWindow>, Option<UsageCredits>), UsageError>;

pub(super) async fn fetch(base_url: &str, token: &str, stored_account: Option<&str>, provider: &str) -> CodexFetched {
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
    let plan = ["plan_type", "tier"].iter().find_map(|key| body[key].as_str().filter(|plan| !plan.is_empty()));
    Ok((plan.map(str::to_string), windows(&body, crate::now_ms()), credits(&body)))
}

fn credits(body: &Value) -> Option<UsageCredits> {
    let credits = body["credits"].as_object()?;
    let flag = |key: &str| credits.get(key).and_then(Value::as_bool) == Some(true);
    let balance = credits.get("balance").and_then(Value::as_str).map(str::to_string);
    Some(UsageCredits { available: flag("has_credits"), unlimited: flag("unlimited"), balance })
}

/// `reset_at` (epoch), else `reset_after_seconds` from `now`, in Unix milliseconds.
fn reset_ms(window: &Value, now: i64) -> Option<i64> {
    let at = number(&window["reset_at"]).and_then(|at| ms_from_f64(if at < 1e11 { at * 1000.0 } else { at }));
    let after = number(&window["reset_after_seconds"]).filter(|seconds| *seconds >= 0.0);
    at.or_else(|| now.checked_add(ms_from_f64(after? * 1000.0)?))
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

fn rate_limit(limit: &Value, prefix: Option<&str>, now: i64, out: &mut Vec<UsageWindow>) {
    for (slot, fallback) in [("primary_window", "Primary"), ("secondary_window", "Secondary")] {
        let window = &limit[slot];
        let Some(used) = number(&window["used_percent"]) else { continue };
        let reported = number(&window["limit_window_seconds"]).filter(|seconds| *seconds > 0.0);
        let minutes = reported.map(|seconds| (seconds / 60.0).round() as u64);
        let exact = |seconds: &f64| seconds.fract() == 0.0 && *seconds <= f64::from(u32::MAX);
        let seconds = reported.filter(exact).map(|seconds| seconds as u64);
        let label = duration_label(minutes, fallback);
        let label = prefix.map_or_else(|| label.clone(), |prefix| format!("{prefix} {label}"));
        let ms = reset_ms(window, now);
        let resets_at = ms.map(|ms| iso_from_unix(ms.div_euclid(1000)));
        let id = prefix.map_or_else(|| slot.to_string(), |prefix| format!("{prefix}/{slot}"));
        let window = UsageWindow::new(label, used, resets_at, minutes).with_seconds(seconds);
        out.push(window.with_id(id).with_reset_ms(ms));
    }
}

/// `rate_limit` windows, then each `additional_rate_limits` entry under its name.
pub(super) fn windows(body: &Value, now: i64) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    rate_limit(&body["rate_limit"], None, now, &mut windows);
    for extra in body["additional_rate_limits"].as_array().into_iter().flatten() {
        let name = extra["limit_name"].as_str().or_else(|| extra["metered_feature"].as_str());
        rate_limit(&extra["rate_limit"], Some(name.unwrap_or("Extra")), now, &mut windows);
    }
    windows
}
