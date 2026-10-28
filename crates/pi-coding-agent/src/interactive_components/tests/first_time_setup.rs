//! First-time setup dialog; ports `first-time-setup.test.ts` and `first-time-setup-fork.test.ts`.

use std::collections::HashMap;
use std::path::Path;

use serde_json::json;

use crate::interactive_components::first_time_setup::{
    DISTRIBUTION, DistributionMetadata, FirstTimeSetupComponent, FirstTimeSetupResult, apply_first_time_setup_result,
    should_run_first_time_setup_with,
};
use crate::interactive_components::test_support::{Calls, Harness, keys};
use crate::settings::SettingsManager;
use crate::theme::tests::global_state;

const OFFICIAL: DistributionMetadata<'static> =
    DistributionMetadata { package_name: "@earendil-works/pi-coding-agent", app_name: "pi", config_dir_name: ".pi" };

fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |name| map.get(name).cloned()
}

fn should_run(distribution: &DistributionMetadata<'_>, vars: &[(&str, &str)], path: &Path) -> bool {
    should_run_first_time_setup_with(distribution, env(vars), path)
}

#[test]
fn runs_when_experimental_default_agent_dir_and_no_settings() {
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.json");
    assert!(should_run(&OFFICIAL, &[("PI_EXPERIMENTAL", "1")], &settings_path));
}

#[test]
fn skipped_when_experimental_features_are_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.json");
    assert!(!should_run(&OFFICIAL, &[], &settings_path));
    assert!(!should_run(&OFFICIAL, &[("PI_EXPERIMENTAL", "true")], &settings_path));
}

#[test]
fn skipped_when_a_custom_agent_dir_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.json");
    let agent_dir = dir.path().to_string_lossy().to_string();
    let vars = [("PI_EXPERIMENTAL", "1"), ("PI_CODING_AGENT_DIR", agent_dir.as_str())];
    assert!(!should_run(&OFFICIAL, &vars, &settings_path));
}

#[test]
fn skipped_when_settings_json_already_exists() {
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.json");
    std::fs::write(&settings_path, "{}").unwrap();
    assert!(!should_run(&OFFICIAL, &[("PI_EXPERIMENTAL", "1")], &settings_path));
}

#[test]
fn skipped_for_a_forked_package() {
    let dir = tempfile::tempdir().unwrap();
    let settings_path = dir.path().join("settings.json");
    let fork = DistributionMetadata { package_name: "@example/pi-coding-agent", ..OFFICIAL };
    assert!(!should_run(&fork, &[("PI_EXPERIMENTAL", "1")], &settings_path));
    let rebrand = DistributionMetadata { app_name: "acme", ..OFFICIAL };
    assert!(!should_run(&rebrand, &[("PI_EXPERIMENTAL", "1")], &settings_path));
    let config_dir = DistributionMetadata { config_dir_name: ".acme", ..OFFICIAL };
    assert!(!should_run(&config_dir, &[("PI_EXPERIMENTAL", "1")], &settings_path));
}

#[test]
fn default_build_is_the_official_distribution() {
    if option_env!("PI_PACKAGE_NAME").is_none()
        && option_env!("PI_CONFIG_NAME").is_none()
        && option_env!("PI_CONFIG_DIR").is_none()
    {
        assert_eq!(DISTRIBUTION, OFFICIAL);
    }
}

#[test]
fn analytics_default_to_disabled_without_tracking_id() {
    let manager = SettingsManager::in_memory(json!({}));
    assert!(!manager.enable_analytics());
    assert_eq!(manager.tracking_id(), None);
}

#[test]
fn opting_in_generates_a_tracking_id() {
    let manager = SettingsManager::in_memory(json!({}));
    manager.set_enable_analytics(true);
    assert!(manager.enable_analytics());
    let id = manager.tracking_id().expect("tracking id");
    assert_eq!(id.len(), 36);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase() || c == '-'), "{id}");
}

#[test]
fn opting_out_does_not_generate_a_tracking_id() {
    let manager = SettingsManager::in_memory(json!({}));
    manager.set_enable_analytics(false);
    assert!(!manager.enable_analytics());
    assert_eq!(manager.tracking_id(), None);
}

#[test]
fn toggling_analytics_keeps_the_tracking_id() {
    let manager = SettingsManager::in_memory(json!({}));
    manager.set_enable_analytics(true);
    let tracking_id = manager.tracking_id();
    manager.set_enable_analytics(false);
    manager.set_enable_analytics(true);
    assert_eq!(manager.tracking_id(), tracking_id);
}

#[test]
fn applying_a_result_persists_theme_and_analytics() {
    let manager = SettingsManager::in_memory(json!({}));
    apply_first_time_setup_result(&manager, &FirstTimeSetupResult { theme: "light".into(), share_analytics: true });
    assert_eq!(manager.theme_setting().as_deref(), Some("light"));
    assert!(manager.enable_analytics());
    assert!(manager.tracking_id().is_some());
}

fn trimmed(harness: &Harness<FirstTimeSetupComponent>) -> Vec<String> {
    harness.render(90).into_iter().map(|line| line.trim_end().to_string()).collect()
}

type Recorders = (Calls<String>, Calls<FirstTimeSetupResult>, Calls<()>);

fn mount() -> (Harness<FirstTimeSetupComponent>, Recorders) {
    let (previews, submits, cancels) = (Calls::new(), Calls::new(), Calls::new());
    let (mut preview, submit, cancel) = (previews.recorder(), submits.recorder(), cancels.counter());
    let harness = Harness::new(90, 30, move |_| {
        FirstTimeSetupComponent::new(move |name: &str| preview(name.to_string()), submit, cancel)
    });
    (harness, (previews, submits, cancels))
}

#[test]
fn renders_the_theme_step_first() {
    let _theme = global_state();
    let (harness, _) = mount();
    let lines = trimmed(&harness);
    assert!(lines[0].starts_with("────"), "{lines:?}");
    assert_eq!(&lines[2..6], ["  ▄▄██▄▄", " ▄█▀▀███▄", " ▀██▀███▀", "  ▀▀██▀▀"]);
    assert!(lines.iter().any(|l| l == " Welcome to pi, the minimal coding agent."), "{lines:?}");
    assert!(lines.iter().any(|l| l == " Pick a theme."));
    assert!(lines.iter().any(|l| l == " → System (matches your terminal colors)"));
    assert!(lines.iter().any(|l| l == "   Dark"));
    assert!(lines.iter().any(|l| l == "   Light"));
    assert!(lines.iter().any(|l| l == " ↑↓ navigate  enter continue  escape/ctrl+c skip setup"), "{lines:?}");
}

#[test]
fn moving_previews_themes_and_clamps_at_the_ends() {
    let _theme = global_state();
    let (harness, (previews, _, _)) = mount();
    harness.send(keys::UP);
    assert!(previews.is_empty());
    harness.send(keys::DOWN);
    assert!(harness.screen().contains("→ Dark"));
    harness.send("j");
    harness.send(keys::DOWN);
    assert_eq!(previews.get(), ["dark", "light"]);
    harness.send("k");
    assert_eq!(previews.get(), ["dark", "light", "dark"]);
}

#[test]
fn confirm_moves_to_analytics_then_submits() {
    let _theme = global_state();
    let (harness, (_, submits, _)) = mount();
    harness.send(keys::DOWN);
    harness.send(keys::ENTER);
    let lines = trimmed(&harness);
    assert!(lines.iter().any(|l| l == " Opt-in to anonymous usage data sharing?"), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with(" Opting in stores a tracking identifier in settings.json")));
    assert!(lines.iter().any(|l| l == " → Share anonymous usage data"));
    assert!(lines.iter().any(|l| l == "   Don't share"));
    assert!(lines.iter().any(|l| l.contains("enter finish")));
    assert!(submits.is_empty());
    harness.send(keys::DOWN);
    harness.send(keys::DOWN);
    assert!(harness.screen().contains("→ Don't share"));
    harness.send("\n");
    assert_eq!(submits.get(), [FirstTimeSetupResult { theme: "dark".into(), share_analytics: false }]);
}

#[test]
fn submits_defaults_and_cancels_with_escape() {
    let _theme = global_state();
    let (harness, (previews, submits, cancels)) = mount();
    harness.send_all(&[keys::ENTER, keys::UP, keys::ENTER]);
    assert_eq!(submits.get(), [FirstTimeSetupResult { theme: "system".into(), share_analytics: true }]);
    assert!(previews.is_empty());
    harness.send(keys::ESCAPE);
    assert_eq!(cancels.len(), 1);
}
