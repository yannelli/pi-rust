//! Settings submenus: theme, warnings, per-model thinking, and the generic submenus.

use pi_ai::ThinkingLevel;
use pi_tui::Component;
use pi_tui::components::SelectItem;

use super::{list_text, preview_recorder, reasoning_model, send, type_label};
use crate::interactive_components::settings_selector::{
    SelectSubmenu, SelectSubmenuOptions, SettingsCallbacks, SettingsConfig, SettingsSelector, StepContext, StepText,
    SteppedSubmenu, SteppedSubmenuOptions, SteppedSubmenuStep,
};
use crate::interactive_components::test_support::{Calls, Harness, keys, render_text, setup};
use crate::theme::TerminalTheme;

fn theme_selector(current: &str, previews: &Calls<String>, changes: &Calls<String>) -> SettingsSelector {
    let changes = changes.clone();
    let config = SettingsConfig {
        current_theme: current.into(),
        terminal_theme: TerminalTheme::Light,
        available_themes: vec!["system".into(), "dark".into(), "light".into()],
        ..SettingsConfig::default()
    };
    let callbacks = SettingsCallbacks {
        on_theme_preview: preview_recorder(previews),
        on_theme_change: Box::new(move |setting: &str| changes.push(setting.to_string())),
        ..SettingsCallbacks::default()
    };
    let selector = SettingsSelector::new(config, callbacks);
    selector.settings_list().borrow_mut().select_item("theme");
    selector
}

#[test]
fn theme_submenu_previews_applies_and_reverts() {
    let _kb = setup();
    let (previews, changes) = (Calls::new(), Calls::new());
    let mut selector = theme_selector("dark", &previews, &changes);
    send(&mut selector, &[keys::ENTER, keys::DOWN]);
    assert_eq!(previews.take(), ["light"]);
    send(&mut selector, &[keys::ESCAPE]);
    assert_eq!(previews.take(), ["dark"]);
    assert!(changes.is_empty());
    assert!(list_text(&selector, 100).contains("→ Theme"), "{}", list_text(&selector, 100));

    send(&mut selector, &[keys::ENTER, keys::UP, keys::UP]);
    assert_eq!(previews.take(), ["dark/dark", "system"]);
    send(&mut selector, &[keys::ENTER]);
    assert_eq!(changes.take(), ["system"]);
    let text = list_text(&selector, 100);
    assert!(text.contains("Theme") && text.contains("system"), "{text}");
}

#[test]
fn automatic_theme_mode_applies_a_light_dark_pair() {
    let _kb = setup();
    let (previews, changes) = (Calls::new(), Calls::new());
    let mut selector = theme_selector("dark", &previews, &changes);
    send(&mut selector, &[keys::ENTER, keys::UP]);
    assert_eq!(previews.take(), ["dark/dark"]);
    send(&mut selector, &[keys::ENTER]);
    assert_eq!(previews.take(), ["dark/dark"]);
    let text = list_text(&selector, 100);
    assert!(text.contains("Automatic Theme"), "{text}");
    assert!(text.contains("Light/dark detection requires terminal support."), "{text}");
    assert!(text.contains("→ Light theme"), "{text}");

    send(&mut selector, &[keys::ENTER]);
    assert!(list_text(&selector, 100).contains("Light Theme"));
    send(&mut selector, &[keys::DOWN, keys::ENTER]);
    assert_eq!(previews.take(), ["light", "light/dark"]);
    let text = list_text(&selector, 100);
    assert!(text.contains("Light theme") && text.contains("light"), "{text}");

    send(&mut selector, &[keys::DOWN, keys::DOWN, keys::ENTER]);
    assert_eq!(changes.take(), ["light/dark"]);
    assert!(list_text(&selector, 100).contains("light/dark"));
}

#[test]
fn automatic_theme_mode_switches_back_to_a_single_theme() {
    let _kb = setup();
    let (previews, changes) = (Calls::new(), Calls::new());
    let mut selector = theme_selector("dark/light", &previews, &changes);
    send(&mut selector, &[keys::ENTER]);
    assert!(list_text(&selector, 100).contains("Automatic Theme"));
    send(&mut selector, &[keys::UP, keys::ENTER]);
    assert_eq!(previews.take(), ["dark"]);
    let text = list_text(&selector, 100);
    assert!(text.contains("Select a theme, or choose automatic"), "{text}");
    assert!(text.contains("→ ✓ dark"), "{text}");
    send(&mut selector, &[keys::ESCAPE]);
    assert_eq!(previews.take(), ["dark/light"]);
    assert!(changes.is_empty());
}

#[test]
fn model_thinking_submenu_sets_and_clears_overrides() {
    let _kb = setup();
    let (fast, deep) = (reasoning_model("fast", "zeta"), reasoning_model("deep", "alpha"));
    let current = reasoning_model("current", "mid");
    let config = SettingsConfig {
        default_model: "zeta/fast".into(),
        current_model: Some(current.clone()),
        available_default_models: vec![fast, deep, current],
        thinking_level: ThinkingLevel::Low,
        ..SettingsConfig::default()
    };
    let (changes, removals) = (Calls::new(), Calls::new());
    let (change, remove) = (changes.clone(), removals.clone());
    let callbacks = SettingsCallbacks {
        on_model_thinking_level_change: Box::new(move |provider: &str, id: &str, level| {
            change.push(format!("{provider}/{id}={level:?}"))
        }),
        on_model_thinking_level_remove: Box::new(move |provider: &str, id: &str| {
            remove.push(format!("{provider}/{id}"))
        }),
        ..SettingsCallbacks::default()
    };
    let mut selector = SettingsSelector::new(config, callbacks);
    selector.settings_list().borrow_mut().select_item("model-thinking");
    assert!(list_text(&selector, 100).contains("Default thinking level per model  none"));
    send(&mut selector, &[keys::ENTER]);
    let text = list_text(&selector, 100);
    let order: Vec<usize> = ["current [mid]", "fast [zeta]", "deep [alpha]"]
        .iter()
        .map(|label| text.find(label).unwrap_or_else(|| panic!("{label} in {text}")))
        .collect();
    assert!(order[0] < order[1] && order[1] < order[2], "{text}");
    assert!(text.contains("Step 1/2 · Select a model to configure"), "{text}");
    assert!(text.contains("Type to filter · Enter to select · Esc to go back"), "{text}");

    type_label(&mut selector, "deep");
    send(&mut selector, &[keys::ENTER]);
    let text = list_text(&selector, 100);
    assert!(text.contains("Thinking Level for deep [alpha]"), "{text}");
    assert!(!text.contains("(clear override)"), "{text}");
    send(&mut selector, &[keys::DOWN, keys::ENTER]);
    assert_eq!(changes.take(), ["alpha/deep=Minimal"]);
    assert!(list_text(&selector, 100).contains("Per-Model Thinking Level"));

    type_label(&mut selector, "deep");
    send(&mut selector, &[keys::ENTER]);
    let text = list_text(&selector, 100);
    assert!(text.contains("→ ✓ minimal"), "{text}");
    assert!(text.contains("Revert to global default (low)"), "{text}");
    send(&mut selector, &[keys::UP, keys::UP, keys::ENTER]);
    assert_eq!(removals.get(), ["alpha/deep"]);

    send(&mut selector, &[keys::DOWN, keys::ENTER, keys::ENTER, keys::ESCAPE, keys::ESCAPE]);
    assert_eq!(changes.take(), ["zeta/fast=Off"]);
    assert!(list_text(&selector, 100).contains("Default thinking level per model  1 configured"));
}

/// `a` and `b`, after the first selection when `key` is `second`.
fn step(key: &str, title: StepText) -> SteppedSubmenuStep {
    let second = key == "second";
    SteppedSubmenuStep {
        key: key.into(),
        title,
        description: "Pick one".into(),
        options: Box::new(move |context: &StepContext| {
            let prefix = if second { context.get("first").cloned().unwrap_or_default() } else { String::new() };
            ["a", "b"]
                .iter()
                .map(|letter| SelectItem::new(&format!("{prefix}{letter}"), &format!("{prefix}{letter}")))
                .collect()
        }),
        preselect: None,
        searchable: false,
        layout: None,
    }
}

#[test]
fn stepped_submenu_goes_back_completes_and_closes() {
    let _kb = setup();
    let (completed, cancelled) = (Calls::new(), Calls::new());
    let record = completed.clone();
    let steps = vec![
        step("first", "First".into()),
        step("second", StepText::Dynamic(Box::new(|context| format!("After {}", context["first"])))),
    ];
    let mut submenu = SteppedSubmenu::new(
        steps,
        move |context: &StepContext| record.push(format!("{}+{}", context["first"], context["second"])),
        cancelled.counter(),
        SteppedSubmenuOptions::default(),
    );
    assert!(render_text(&mut submenu, 80).contains("Step 1/2 · Pick one"));
    submenu.handle_input(keys::DOWN);
    submenu.handle_input(keys::ENTER);
    let text = render_text(&mut submenu, 80);
    assert!(text.contains("After b") && text.contains("Step 2/2 · Pick one") && text.contains("→ ba"), "{text}");
    submenu.handle_input(keys::ESCAPE);
    assert!(render_text(&mut submenu, 80).contains("First"));
    submenu.handle_input(keys::ENTER);
    submenu.handle_input(keys::DOWN);
    submenu.handle_input(keys::ENTER);
    assert_eq!(completed.get(), ["a+ab"]);
    assert_eq!(cancelled.len(), 1);

    let options = SteppedSubmenuOptions {
        start_at_step: 1,
        initial_context: StepContext::from([("first".to_string(), "x".to_string())]),
        loop_back: false,
    };
    let (record, cancelled) = (completed.clone(), Calls::new());
    let steps = vec![step("first", "First".into()), step("second", "Second".into())];
    let on_complete = move |context: &StepContext| record.push(context["second"].clone());
    let mut submenu = SteppedSubmenu::new(steps, on_complete, cancelled.counter(), options);
    assert!(render_text(&mut submenu, 80).contains("→ xa"));
    submenu.handle_input(keys::ESCAPE);
    submenu.handle_input(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn select_submenu_filters_when_searchable() {
    let options: Vec<SelectItem> = ["alpha", "beta", "gamma"]
        .iter()
        .map(|name| SelectItem::new(name, name).with_description(&format!("{name} item")))
        .collect();
    let selected = Calls::new();
    let on_select = selected.clone();
    let harness = Harness::new(80, 20, move |_| {
        SelectSubmenu::new(
            "Pick",
            "Choose a letter",
            options,
            "beta",
            move |value: &str| on_select.push(value.to_string()),
            || {},
            None,
            SelectSubmenuOptions { searchable: true, layout: None },
        )
    });
    let screen = harness.screen();
    assert!(screen.contains("Pick") && screen.contains("Choose a letter"), "{screen}");
    assert!(screen.contains("→ beta"), "{screen}");
    assert!(screen.contains("Type to filter · Enter to select · Esc to go back"), "{screen}");
    harness.type_text("gam");
    let screen = harness.screen();
    assert!(screen.contains("→ gamma") && !screen.contains("alpha"), "{screen}");
    harness.send(keys::ENTER);
    assert_eq!(selected.get(), ["gamma"]);
}

#[test]
fn pages_a_searchable_submenu_with_selection_page_keys() {
    let _kb = setup();
    let options: Vec<SelectItem> =
        (0..20).map(|i| SelectItem::new(&format!("theme-{i}"), &format!("theme-{i}"))).collect();
    let selected = Calls::new();
    let on_select = selected.clone();
    let mut submenu = SelectSubmenu::new(
        "Theme",
        "",
        options,
        "theme-0",
        move |value: &str| on_select.push(value.to_string()),
        || {},
        None,
        SelectSubmenuOptions { searchable: true, layout: None },
    );
    submenu.handle_input(keys::PAGE_DOWN);
    submenu.handle_input(keys::ENTER);
    assert_eq!(selected.get(), ["theme-10"]);
}

#[test]
fn provider_submenu_toggles_filters_and_keeps_changes_after_escape() {
    let _kb = setup();
    let changed = Calls::new();
    let config = SettingsConfig {
        available_providers: vec![("alpha".into(), "Alpha Cloud".into()), ("beta".into(), "beta".into())],
        disabled_providers: vec!["unavailable".into()],
        ..SettingsConfig::default()
    };
    let callbacks = SettingsCallbacks {
        on_disabled_providers_change: Box::new(changed.recorder()),
        ..SettingsCallbacks::default()
    };
    let mut selector = SettingsSelector::new(config, callbacks);
    selector.settings_list().borrow_mut().select_item("disabled-providers");
    send(&mut selector, &[keys::ENTER]);
    let text = list_text(&selector, 100);
    assert!(text.contains("alpha (Alpha Cloud)") && text.contains("unavailable"), "{text}");
    send(&mut selector, &[keys::SPACE, keys::DOWN, keys::ENTER]);
    assert_eq!(changed.take(), [vec!["alpha", "unavailable"], vec!["alpha", "beta", "unavailable"]]);
    send(&mut selector, &[keys::ESCAPE]);
    assert!(list_text(&selector, 100).contains("3 disabled"));
    send(&mut selector, &[keys::ENTER]);
    let text = list_text(&selector, 100);
    assert!(text.lines().find(|line| line.contains("alpha")).unwrap().contains("disabled"), "{text}");
    type_label(&mut selector, "unavailable");
    let text = list_text(&selector, 100);
    assert!(!text.contains("Alpha Cloud"), "{text}");
    send(&mut selector, &[keys::ENTER, keys::ESCAPE]);
    assert_eq!(changed.get(), [vec!["alpha", "beta"]]);
    assert!(list_text(&selector, 100).contains("2 disabled"));
}

#[test]
fn provider_submenu_persists_and_returns_to_none() {
    use std::sync::Arc;

    use crate::settings::SettingsManager;

    let _kb = setup();
    let settings = Arc::new(SettingsManager::in_memory(serde_json::json!({})));
    let config =
        SettingsConfig { available_providers: vec![("local".into(), "Local".into())], ..SettingsConfig::default() };
    let mut selector = SettingsSelector::new(config, SettingsCallbacks::persisting(settings.clone()));
    selector.settings_list().borrow_mut().select_item("disabled-providers");
    send(&mut selector, &[keys::ENTER, keys::ENTER, keys::ESCAPE]);
    assert_eq!(settings.disabled_providers(), ["local"]);
    assert!(list_text(&selector, 100).contains("1 disabled"));
    send(&mut selector, &[keys::ENTER, keys::ENTER, keys::ESCAPE]);
    assert!(settings.disabled_providers().is_empty());
    assert!(list_text(&selector, 100).contains("none"));
}
