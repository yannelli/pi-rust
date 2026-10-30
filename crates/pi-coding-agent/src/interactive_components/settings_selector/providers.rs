use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use pi_tui::components::SettingsList;
use pi_tui::components::settings_list::{SettingsListOptions, SubmenuFactory};

use super::SharedCallbacks;
use super::config::SettingsConfig;
use super::items::item;
use crate::theme::get_settings_list_theme;

pub(super) fn summary(disabled: &[String]) -> String {
    if disabled.is_empty() { "none".into() } else { format!("{} disabled", disabled.len()) }
}

pub(super) fn factory(config: &SettingsConfig, callbacks: SharedCallbacks) -> SubmenuFactory {
    let disabled = Rc::new(RefCell::new(config.disabled_providers.clone()));
    let mut providers: BTreeMap<String, String> = config.available_providers.iter().cloned().collect();
    for provider in &config.disabled_providers {
        providers.entry(provider.clone()).or_insert_with(|| provider.clone());
    }
    Rc::new(move |_, done| {
        let items = providers
            .iter()
            .map(|(id, name)| {
                let label = if id == name { id.clone() } else { format!("{id} ({name})") };
                let current = if disabled.borrow().contains(id) { "disabled" } else { "enabled" };
                item(
                    id,
                    &label,
                    "Changes apply now and save to global settings",
                    current.into(),
                    &["enabled", "disabled"],
                )
            })
            .collect();
        let (changed, cancelled) = (disabled.clone(), disabled.clone());
        let callbacks = callbacks.clone();
        Rc::new(RefCell::new(SettingsList::new(
            items,
            10,
            get_settings_list_theme(),
            move |id, value| {
                let mut providers = changed.borrow_mut();
                providers.retain(|provider| provider != id);
                if value == "disabled" {
                    providers.push(id.to_string());
                }
                providers.sort();
                providers.dedup();
                (callbacks.borrow_mut().on_disabled_providers_change)(providers.clone());
            },
            move || done.done(Some(&summary(&cancelled.borrow()))),
            SettingsListOptions { enable_search: true },
        )))
    })
}
