//! Claude subscription usage: `GET /api/oauth/usage` with the OAuth headers of the provider.

use serde_json::Value;

use super::time::normalize;
use super::{Fetched, UsageWindow, json_response, number};
use crate::providers::anthropic::{CLAUDE_CODE_VERSION, OAUTH_API_BETA};

const FIVE_HOURS: u64 = 300;
const WEEK: u64 = 10_080;

pub(super) async fn fetch(base_url: &str, token: &str) -> Fetched {
    let request = crate::http::shared_client()
        .get(format!("{}/api/oauth/usage", base_url.trim_end_matches('/')))
        .bearer_auth(token)
        .header("accept", "application/json")
        .header("anthropic-beta", OAUTH_API_BETA)
        .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
        .header("x-app", "cli");
    let body = json_response(request, "anthropic").await?;
    Ok((None, windows(&body)))
}

fn window(label: &str, value: &Value, minutes: u64) -> Option<UsageWindow> {
    let used = number(&value["utilization"])?;
    let resets_at = value["resets_at"].as_str().and_then(normalize);
    Some(UsageWindow::new(label, used, resets_at, Some(minutes)))
}

fn title(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// `five_hour`, `seven_day`, the `seven_day_<model>` keys, and `weekly_scoped` entries of `limits`.
pub(super) fn windows(body: &Value) -> Vec<UsageWindow> {
    let mut windows: Vec<UsageWindow> = [("five_hour", "5h", FIVE_HOURS), ("seven_day", "Weekly", WEEK)]
        .iter()
        .filter_map(|(key, label, minutes)| window(label, &body[key], *minutes))
        .collect();
    let object = body.as_object().into_iter().flatten();
    let scoped = object.filter_map(|(key, value)| Some((key.strip_prefix("seven_day_")?, value)));
    for (model, value) in scoped.filter(|(model, value)| !model.is_empty() && value.is_object()) {
        windows.extend(window(&format!("Weekly {}", title(model)), value, WEEK));
    }
    for limit in body["limits"].as_array().into_iter().flatten() {
        let scope = &limit["scope"];
        let name = ["model", "surface"].iter().find_map(|kind| {
            let label = &scope[kind];
            label["display_name"].as_str().or_else(|| label["id"].as_str()).filter(|name| !name.is_empty())
        });
        let (Some(name), Some(used)) = (name, number(&limit["percent"])) else { continue };
        if limit["kind"] != "weekly_scoped" {
            continue;
        }
        let label = format!("Weekly {name}");
        if windows.iter().any(|window| window.label.eq_ignore_ascii_case(&label)) {
            continue;
        }
        let resets_at = limit["resets_at"].as_str().and_then(normalize);
        windows.push(UsageWindow::new(label, used, resets_at, Some(WEEK)));
    }
    windows
}
