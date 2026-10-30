use std::sync::Arc;

use serde_json::json;

use super::{list_text, send};
use crate::interactive_components::settings_selector::{SettingsCallbacks, SettingsConfig, SettingsSelector};
use crate::interactive_components::test_support::{Calls, keys, setup};
use crate::settings::{ProviderRetryConfig, RetryConfig, SettingsManager};

#[test]
fn retry_controls_dispatch_values_and_defaults() {
    let _kb = setup();
    let (enabled, retries, base, delay) = (Calls::new(), Calls::new(), Calls::new(), Calls::new());
    let (timeout, provider_retries, server_delay) = (Calls::new(), Calls::new(), Calls::new());
    let callbacks = SettingsCallbacks {
        on_retry_enabled_change: Box::new(enabled.recorder()),
        on_retry_max_retries_change: Box::new(retries.recorder()),
        on_retry_base_delay_ms_change: Box::new(base.recorder()),
        on_retry_max_agent_delay_ms_change: Box::new(delay.recorder()),
        on_provider_timeout_ms_change: Box::new(timeout.recorder()),
        on_provider_max_retries_change: Box::new(provider_retries.recorder()),
        on_provider_max_retry_delay_ms_change: Box::new(server_delay.recorder()),
        ..SettingsCallbacks::default()
    };
    let mut selector = SettingsSelector::new(SettingsConfig::default(), callbacks);
    for (id, count) in [
        ("auto-retry", 2),
        ("retry-max-retries", 3),
        ("retry-base-delay", 3),
        ("retry-max-delay", 3),
        ("provider-timeout", 7),
        ("provider-max-retries", 6),
        ("provider-max-retry-delay", 3),
    ] {
        selector.settings_list().borrow_mut().select_item(id);
        for _ in 0..count {
            send(&mut selector, &[keys::ENTER]);
        }
    }
    assert_eq!(enabled.get(), [false, true]);
    assert_eq!(retries.get(), [5, 10, 0]);
    assert_eq!(base.get(), [5_000, 10_000, 1_000]);
    assert_eq!(delay.get(), [120_000, 300_000, 30_000]);
    assert_eq!(
        timeout.get(),
        [Some(30_000), Some(60_000), Some(120_000), Some(300_000), Some(600_000), None, Some(30_000)]
    );
    assert_eq!(provider_retries.get(), [Some(0), Some(1), Some(2), Some(3), Some(5), None]);
    assert_eq!(server_delay.get(), [120_000, 300_000, 0]);
}

#[test]
fn custom_retry_values_stay_selected_and_in_the_cycle() {
    let _kb = setup();
    let config = SettingsConfig {
        retry: RetryConfig { enabled: false, max_retries: 7, base_delay_ms: 7_000, max_agent_delay_ms: 45_500 },
        provider_retry: ProviderRetryConfig {
            timeout_ms: Some(7_000),
            max_retries: Some(4),
            max_retry_delay_ms: 75_000,
        },
        ..SettingsConfig::default()
    };
    let mut selector = SettingsSelector::new(config, SettingsCallbacks::default());
    for (id, label, count) in [
        ("retry-max-retries", "7", 7),
        ("retry-base-delay", "7 sec", 5),
        ("retry-max-delay", "45500 ms", 5),
        ("provider-timeout", "7 sec", 7),
        ("provider-max-retries", "4", 7),
        ("provider-max-retry-delay", "75 sec", 6),
    ] {
        let list = selector.settings_list();
        list.borrow_mut().select_item(id);
        let current = || list.borrow().items().iter().find(|item| item.id == id).unwrap().current_value.clone();
        assert_eq!(current(), label);
        assert!(list_text(&selector, 100).contains(label));
        for _ in 0..count {
            send(&mut selector, &[keys::ENTER]);
        }
        assert_eq!(current(), label);
    }
}

#[test]
fn retry_controls_persist_and_reload_config() {
    let _theme = crate::theme::tests::global_state();
    let _kb = setup();
    let settings = Arc::new(SettingsManager::in_memory(json!({})));
    let config = SettingsConfig::from_settings(&settings);
    let mut selector = SettingsSelector::new(config, SettingsCallbacks::persisting(settings.clone()));
    for id in [
        "auto-retry",
        "retry-max-retries",
        "retry-base-delay",
        "retry-max-delay",
        "provider-timeout",
        "provider-max-retries",
        "provider-max-retry-delay",
    ] {
        selector.settings_list().borrow_mut().select_item(id);
        send(&mut selector, &[keys::ENTER]);
    }
    let config = SettingsConfig::from_settings(&settings);
    assert_eq!(
        config.retry,
        RetryConfig { enabled: false, max_retries: 5, base_delay_ms: 5_000, max_agent_delay_ms: 120_000 }
    );
    assert_eq!(
        config.provider_retry,
        ProviderRetryConfig { timeout_ms: Some(30_000), max_retries: Some(0), max_retry_delay_ms: 120_000 }
    );
    for (id, count) in [("provider-timeout", 5), ("provider-max-retries", 5)] {
        selector.settings_list().borrow_mut().select_item(id);
        for _ in 0..count {
            send(&mut selector, &[keys::ENTER]);
        }
    }
    let config = SettingsConfig::from_settings(&settings);
    assert_eq!(config.provider_retry.timeout_ms, None);
    assert_eq!(config.provider_retry.max_retries, None);
    assert_eq!(config.provider_retry.max_retry_delay_ms, 120_000);
    assert_eq!(config.retry.max_retries, 5);
}

#[test]
fn server_retry_delay_zero_shows_and_cycles_no_limit() {
    let _theme = crate::theme::tests::global_state();
    let _kb = setup();
    let settings = Arc::new(SettingsManager::in_memory(json!({ "retry": { "provider": { "maxRetryDelayMs": 0 } } })));
    let config = SettingsConfig::from_settings(&settings);
    assert_eq!(config.provider_retry.max_retry_delay_ms, 0);
    let mut selector = SettingsSelector::new(config, SettingsCallbacks::persisting(settings.clone()));
    let list = selector.settings_list();
    list.borrow_mut().select_item("provider-max-retry-delay");
    assert!(list_text(&selector, 100).contains("no limit"));
    send(&mut selector, &[keys::ENTER]);
    assert_eq!(SettingsConfig::from_settings(&settings).provider_retry.max_retry_delay_ms, 30_000);
    for _ in 0..4 {
        send(&mut selector, &[keys::ENTER]);
    }
    assert_eq!(SettingsConfig::from_settings(&settings).provider_retry.max_retry_delay_ms, 0);
}
