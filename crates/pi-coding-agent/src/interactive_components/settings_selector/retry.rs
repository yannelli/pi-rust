use pi_tui::components::settings_list::SettingItem;

use super::config::{SettingsCallbacks, SettingsConfig};
use super::items::item;

const BASE_DELAYS: &[u64] = &[1_000, 2_000, 5_000, 10_000];
const MAX_DELAYS: &[u64] = &[30_000, 60_000, 120_000, 300_000];
const TIMEOUTS: &[u64] = &[30_000, 60_000, 120_000, 300_000, 600_000];
const NO_LIMIT: &str = "no limit";

fn duration(ms: u64) -> String {
    if ms > 0 && ms % 60_000 == 0 {
        format!("{} min", ms / 60_000)
    } else if ms % 1_000 == 0 {
        format!("{} sec", ms / 1_000)
    } else {
        format!("{} ms", ms)
    }
}

fn parse_duration(value: &str) -> Option<u64> {
    let (number, unit) = value.split_once(' ')?;
    let multiplier = match unit {
        "ms" => 1,
        "sec" => 1_000,
        "min" => 60_000,
        _ => return None,
    };
    number.parse::<u64>().ok()?.checked_mul(multiplier)
}

fn choice(id: &str, label: &str, description: &str, current: String, mut values: Vec<String>) -> SettingItem {
    if !values.contains(&current) {
        values.push(current.clone());
    }
    let values: Vec<_> = values.iter().map(String::as_str).collect();
    item(id, label, description, current, &values)
}

fn server_delay(ms: u64) -> String {
    if ms == 0 { NO_LIMIT.into() } else { duration(ms) }
}

fn durations(values: &[u64]) -> Vec<String> {
    values.iter().copied().map(duration).collect()
}

pub(super) fn build_items(config: &SettingsConfig) -> Vec<SettingItem> {
    let retry = config.retry;
    let provider = config.provider_retry;
    vec![
        item("auto-retry", "Auto-retry", "Retry failed agent requests", retry.enabled.to_string(), &["true", "false"]),
        choice(
            "retry-max-retries",
            "Max retries",
            "Maximum retries for a failed agent request",
            retry.max_retries.to_string(),
            [0, 1, 2, 3, 5, 10].map(|n| n.to_string()).to_vec(),
        ),
        choice(
            "retry-base-delay",
            "Retry base delay",
            "Initial wait before an agent retry",
            duration(retry.base_delay_ms),
            durations(BASE_DELAYS),
        ),
        choice(
            "retry-max-delay",
            "Max retry wait",
            "Maximum wait between agent retries",
            duration(retry.max_agent_delay_ms),
            durations(MAX_DELAYS),
        ),
        choice(
            "provider-timeout",
            "Provider request timeout",
            "Request timeout; default uses httpIdleTimeoutMs, or the provider's timeout when that is 0",
            provider.timeout_ms.map_or_else(|| "default".into(), duration),
            std::iter::once("default".into()).chain(durations(TIMEOUTS)).collect(),
        ),
        choice(
            "provider-max-retries",
            "Provider max retries",
            "Retries within a provider request; default uses the provider's limit",
            provider.max_retries.map_or_else(|| "default".into(), |n| n.to_string()),
            ["default", "0", "1", "2", "3", "5"].map(String::from).to_vec(),
        ),
        choice(
            "provider-max-retry-delay",
            "Max server retry delay",
            "Maximum server retry delay the provider will accept",
            server_delay(provider.max_retry_delay_ms),
            durations(MAX_DELAYS).into_iter().chain(std::iter::once(NO_LIMIT.into())).collect(),
        ),
    ]
}

pub(super) fn dispatch_change(callbacks: &mut SettingsCallbacks, id: &str, value: &str) {
    match id {
        "auto-retry" => (callbacks.on_retry_enabled_change)(value == "true"),
        "retry-max-retries" => value.parse().ok().into_iter().for_each(&mut callbacks.on_retry_max_retries_change),
        "retry-base-delay" => parse_duration(value).into_iter().for_each(&mut callbacks.on_retry_base_delay_ms_change),
        "retry-max-delay" => {
            parse_duration(value).into_iter().for_each(&mut callbacks.on_retry_max_agent_delay_ms_change)
        }
        "provider-timeout" => {
            if value == "default" {
                (callbacks.on_provider_timeout_ms_change)(None);
            } else if let Some(timeout) = parse_duration(value) {
                (callbacks.on_provider_timeout_ms_change)(Some(timeout));
            }
        }
        "provider-max-retries" => {
            if value == "default" {
                (callbacks.on_provider_max_retries_change)(None);
            } else if let Ok(retries) = value.parse() {
                (callbacks.on_provider_max_retries_change)(Some(retries));
            }
        }
        "provider-max-retry-delay" => {
            let delay = if value == NO_LIMIT { Some(0) } else { parse_duration(value) };
            delay.into_iter().for_each(&mut callbacks.on_provider_max_retry_delay_ms_change)
        }
        _ => {}
    }
}
