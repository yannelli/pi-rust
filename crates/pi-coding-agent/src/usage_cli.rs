//! Subscription usage report for `/usage` and `pi usage [provider] [--json]`.

use std::path::Path;
use std::sync::Arc;

use pi_ai::auth::{FileCredentialStore, OAuthLookup};
use pi_ai::usage::{ProviderUsage, UsageService, unix_from_iso};

use crate::auth_cli::auth_context;
use crate::cli::CommandOutput;
use crate::config::auth_path;

const BAR_CELLS: usize = 10;
pub const USAGE_HELP: &str = "Usage: pi usage [provider] [--json]

Show the subscription limits (5-hour, weekly, billing cycle) of the providers you are logged in to:
anthropic, openai-codex, cursor, xai (SuperGrok and Grok Build), and google-antigravity.";

/// How a part of the report is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Title,
    Label,
    Ok,
    Warn,
    High,
    Error,
}

fn provider_name(provider: &str) -> &str {
    match provider {
        "anthropic" => "Claude",
        "openai-codex" => "ChatGPT Codex",
        "cursor" => "Cursor",
        "xai" => "SuperGrok",
        "google-antigravity" => "Google Antigravity",
        other => other,
    }
}

/// `3d 4h`, `2h 13m`, or `12m` until `seconds`.
pub fn time_until(seconds: i64, now: i64) -> String {
    let left = (seconds - now).max(0) / 60;
    let (days, hours, minutes) = (left / 1440, left / 60 % 24, left % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

fn tone(percent: f64) -> Tone {
    if percent >= 90.0 {
        Tone::High
    } else if percent >= 70.0 {
        Tone::Warn
    } else {
        Tone::Ok
    }
}

/// The report text; `now` is Unix seconds.
pub fn report_text(usages: &[ProviderUsage], now: i64, style: &dyn Fn(Tone, &str) -> String) -> String {
    let mut text = style(Tone::Title, "Subscription usage");
    if usages.is_empty() {
        text.push_str(
            "\n\nNo login with a usage report. Run /login for anthropic, openai-codex, cursor, xai, or google-antigravity.",
        );
        return text;
    }
    let width = usages.iter().flat_map(|usage| &usage.windows).map(|window| window.label.chars().count()).max();
    let width = width.unwrap_or(0);
    for usage in usages {
        let plan = usage.plan.as_deref().map(|plan| format!(" · {plan}")).unwrap_or_default();
        let heading = format!("{} ({}){plan}", provider_name(&usage.provider), usage.provider);
        text.push_str(&format!("\n\n{}", style(Tone::Title, &heading)));
        if let Some(error) = &usage.error {
            text.push_str(&format!("\n  {}", style(Tone::Error, error)));
            continue;
        }
        if usage.windows.is_empty() {
            text.push_str(&format!("\n  {}", style(Tone::Label, "No limits reported")));
        }
        for window in &usage.windows {
            let percent = window.used_percent;
            let filled = ((percent / 100.0 * BAR_CELLS as f64).round() as usize).min(BAR_CELLS);
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_CELLS - filled));
            let reset = window.resets_at.as_deref().and_then(unix_from_iso);
            let reset = reset.map(|at| format!("  resets in {}", time_until(at, now))).unwrap_or_default();
            let label = format!("{:<width$}", window.label);
            let used = format!("{bar} {percent:>3.0}%");
            text.push_str(&format!("\n  {}  {}{}", style(Tone::Label, &label), style(tone(percent), &used), reset));
        }
    }
    text
}

/// Usage of `provider`, or of every logged-in provider, with the credentials in `agent_dir`.
pub async fn fetch(agent_dir: &Path, oauth: OAuthLookup, provider: Option<&str>) -> Vec<ProviderUsage> {
    let store = Arc::new(FileCredentialStore::new(auth_path(agent_dir)));
    let service = UsageService::new(Arc::new(auth_context(agent_dir, store, oauth).await.resolver));
    match provider {
        Some(provider) => vec![service.get(provider, true).await],
        None => service.all(true).await,
    }
}

/// Run `pi usage ...`; `None` when `args` is another command.
pub async fn run_usage_command(args: &[String], agent_dir: &Path, oauth: OAuthLookup) -> Option<CommandOutput> {
    if args.first().map(String::as_str) != Some("usage") {
        return None;
    }
    let (mut json, mut provider) = (false, None);
    for arg in &args[1..] {
        match arg.as_str() {
            "--help" | "-h" => {
                return Some(CommandOutput { code: 0, stdout: format!("{USAGE_HELP}\n"), ..Default::default() });
            }
            "--json" => json = true,
            flag if flag.starts_with('-') => {
                let stderr = format!("Unknown option {flag} for \"usage\".\n{USAGE_HELP}\n");
                return Some(CommandOutput { code: 1, stderr, ..Default::default() });
            }
            name if provider.is_none() => provider = Some(name.to_string()),
            extra => {
                let stderr = format!("Unexpected argument {extra}.\n{USAGE_HELP}\n");
                return Some(CommandOutput { code: 1, stderr, ..Default::default() });
            }
        }
    }
    let usages = fetch(agent_dir, oauth, provider.as_deref()).await;
    let stdout = if json {
        serde_json::to_string_pretty(&serde_json::json!({ "providers": usages })).unwrap_or_default()
    } else {
        report_text(&usages, pi_ai::now_ms() / 1000, &|_, text| text.to_string())
    };
    let failed = !usages.is_empty() && usages.iter().all(|usage| usage.error.is_some());
    Some(CommandOutput { code: i32::from(failed), stdout: format!("{stdout}\n"), stderr: String::new() })
}

#[cfg(test)]
mod tests {
    use pi_ai::usage::UsageWindow;

    use super::*;

    fn usage(provider: &str, windows: Vec<UsageWindow>, error: Option<&str>) -> ProviderUsage {
        let error = error.map(str::to_string);
        ProviderUsage { provider: provider.into(), plan: None, windows, error, fetched_at: 0 }
    }

    #[test]
    fn renders_windows_with_bars_resets_and_errors() {
        let now = 1_793_379_600 - 2 * 3600 - 13 * 60;
        let windows = vec![
            UsageWindow::new("5h", 42.0, Some("2026-10-30T17:00:00Z".into()), Some(300)),
            UsageWindow::new("Weekly", 95.0, None, Some(10_080)),
        ];
        let usages = [usage("anthropic", windows, None), usage("cursor", Vec::new(), Some("Not logged in."))];
        let text = report_text(&usages, now, &|tone, text| match tone {
            Tone::High => format!("<{text}>"),
            _ => text.to_string(),
        });
        let expected = "Subscription usage\n\nClaude (anthropic)\n  5h      ████░░░░░░  42%  resets in 2h 13m\n  Weekly  <██████████  95%>\n\nCursor (cursor)\n  Not logged in.";
        assert_eq!(text, expected);
        assert_eq!(time_until(now + 3 * 86_400 + 4 * 3600 + 59, now), "3d 4h");
        assert!(report_text(&[], now, &|_, text| text.to_string()).contains("No login"));
    }

    #[tokio::test]
    async fn the_cli_reports_a_missing_login_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let oauth: OAuthLookup = Arc::new(pi_ai::auth::oauth::builtin_oauth);
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert!(run_usage_command(&args(&["auth"]), dir.path(), oauth.clone()).await.is_none());
        let output = run_usage_command(&args(&["usage", "cursor", "--json"]), dir.path(), oauth.clone()).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
        assert_eq!(value["providers"][0]["error"], "Not logged in. Run /login cursor.");
        assert_eq!(output.code, 1);
        let empty = run_usage_command(&args(&["usage"]), dir.path(), oauth.clone()).await.unwrap();
        assert!(empty.stdout.contains("No login with a usage report") && empty.code == 0);
        assert_eq!(run_usage_command(&args(&["usage", "--bogus"]), dir.path(), oauth).await.unwrap().code, 1);
    }
}
