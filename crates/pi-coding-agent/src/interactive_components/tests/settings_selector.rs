//! Settings selector; ports `settings-selector.test.ts`.

mod submenus;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use pi_agent::QueueMode;
use pi_ai::{Model, ThinkingLevel};
use pi_tui::Component;
use serde_json::json;

use crate::interactive_components::settings_selector::{
    DefaultProjectTrust, OnText, SettingsCallbacks, SettingsConfig, SettingsSelector, format_http_idle_timeout_ms,
};
use crate::interactive_components::test_support::{Calls, Harness, keys, plain, setup};
use crate::settings::schema::{FullscreenExitOutput, FullscreenScrollbar, TuiMode};
use crate::settings::{QuietStartup, SettingsManager};
use crate::theme::TerminalTheme;

pub(super) fn list_text(selector: &SettingsSelector, width: usize) -> String {
    plain(&selector.settings_list().borrow_mut().render(width)).join("\n")
}

pub(super) fn send(selector: &mut SettingsSelector, inputs: &[&str]) {
    for input in inputs {
        selector.handle_input(input);
    }
}

pub(super) fn type_label(selector: &mut SettingsSelector, label: &str) {
    for ch in label.chars() {
        selector.handle_input(&ch.to_string());
    }
}

pub(super) fn reasoning_model(id: &str, provider: &str) -> Model {
    Model { id: id.into(), name: id.into(), provider: provider.into(), reasoning: true, ..Model::default() }
}

/// A theme preview callback that records settings.
pub(super) fn preview_recorder(calls: &Calls<String>) -> Option<OnText> {
    let calls = calls.clone();
    Some(Box::new(move |setting: &str| calls.push(setting.to_string())))
}

#[test]
fn cycles_through_fullscreen_settings() {
    let _kb = setup();
    let exit_output = Calls::new();
    let scrollbar = Calls::new();
    let copy_on_select = Calls::new();
    let wheel_lines = Calls::new();
    let config = SettingsConfig {
        fullscreen_exit_output: FullscreenExitOutput::Transcript,
        fullscreen_scrollbar: FullscreenScrollbar::Auto,
        fullscreen_copy_on_select: true,
        fullscreen_wheel_scroll_lines: Some(7),
        ..SettingsConfig::default()
    };
    let cycle = |label: &str, count: usize| {
        let callbacks = SettingsCallbacks {
            on_fullscreen_exit_output_change: Box::new(exit_output.recorder()),
            on_fullscreen_scrollbar_change: Box::new(scrollbar.recorder()),
            on_fullscreen_copy_on_select_change: Box::new(copy_on_select.recorder()),
            on_fullscreen_wheel_scroll_lines_change: Box::new(wheel_lines.recorder()),
            ..SettingsCallbacks::default()
        };
        let list = SettingsSelector::new(config.clone(), callbacks).settings_list();
        for ch in label.chars() {
            list.borrow_mut().handle_input(&ch.to_string());
        }
        for _ in 0..count {
            list.borrow_mut().handle_input(keys::ENTER);
        }
    };

    cycle("Fullscreen exit output", 2);
    assert_eq!(exit_output.get(), [FullscreenExitOutput::ResumeHint, FullscreenExitOutput::Transcript]);
    cycle("Fullscreen scrollbar", 3);
    assert_eq!(scrollbar.get(), [FullscreenScrollbar::Always, FullscreenScrollbar::Hidden, FullscreenScrollbar::Auto]);
    cycle("Fullscreen copy on select", 2);
    assert_eq!(copy_on_select.get(), [false, true]);
    // #9758: custom values from settings.json stay in the cycle.
    cycle("Fullscreen wheel scrolling", 3);
    assert_eq!(wheel_lines.get(), [Some(10), None, Some(1)]);
}

fn theme_config(current: &str, themes: &[&str]) -> SettingsConfig {
    SettingsConfig {
        current_theme: current.into(),
        terminal_theme: TerminalTheme::Dark,
        available_themes: themes.iter().map(|theme| theme.to_string()).collect(),
        ..SettingsConfig::default()
    }
}

#[test]
fn keeps_the_configured_fixed_theme_marked_while_browsing() {
    let _kb = setup();
    let callbacks = SettingsCallbacks { on_theme_preview: Some(Box::new(|_| {})), ..SettingsCallbacks::default() };
    let selector = SettingsSelector::new(theme_config("dark", &["system", "dark", "light"]), callbacks);
    let list = selector.settings_list();
    list.borrow_mut().select_item("theme");
    list.borrow_mut().handle_input(keys::ENTER);
    let output = list_text(&selector, 120);
    let system = output.lines().position(|line| line.starts_with("    system ")).expect(&output);
    let lines: Vec<&str> = output.lines().collect();
    assert!(lines[system].trim_end().ends_with("Theme created from your terminal's colors"), "{output}");
    assert!(lines[system + 1].starts_with("    automatic "), "{output}");
    assert!(lines[system + 1].contains("Use separate themes"), "{output}");
    assert!(output.contains("→ ✓ dark"), "{output}");

    list.borrow_mut().handle_input(keys::DOWN);
    let output = list_text(&selector, 120);
    assert!(output.contains("  ✓ dark"), "{output}");
    assert!(output.contains("→   light"), "{output}");
}

#[test]
fn keeps_a_configured_automatic_theme_marked_while_browsing() {
    let _kb = setup();
    let callbacks = SettingsCallbacks { on_theme_preview: Some(Box::new(|_| {})), ..SettingsCallbacks::default() };
    let selector = SettingsSelector::new(theme_config("light/dark", &["dark", "light", "other"]), callbacks);
    let list = selector.settings_list();
    list.borrow_mut().select_item("theme");
    list.borrow_mut().handle_input(keys::ENTER);
    list.borrow_mut().handle_input(keys::ENTER);
    let output = list_text(&selector, 120);
    assert!(output.contains("→ ✓ light"), "{output}");

    list.borrow_mut().handle_input(keys::DOWN);
    let output = list_text(&selector, 120);
    assert!(output.contains("  ✓ light"), "{output}");
    assert!(output.contains("→   other"), "{output}");
}

#[test]
fn keeps_the_configured_per_model_thinking_level_marked_while_browsing() {
    let _kb = setup();
    let model = reasoning_model("thinking-model", "faux");
    let key = format!("{}/{}", model.provider, model.id);
    let config = SettingsConfig {
        default_model: key.clone(),
        available_default_models: vec![model],
        thinking_level: ThinkingLevel::High,
        model_thinking_levels: BTreeMap::from([(key, ThinkingLevel::Medium)]),
        ..SettingsConfig::default()
    };
    let selector = SettingsSelector::new(config, SettingsCallbacks::default());
    let list = selector.settings_list();
    list.borrow_mut().select_item("model-thinking");
    list.borrow_mut().handle_input(keys::ENTER);
    list.borrow_mut().handle_input(keys::ENTER);

    let output = list_text(&selector, 120);
    assert!(output.contains("→ ✓ medium"), "{output}");
    assert!(output.contains("    (clear override)"), "{output}");

    list.borrow_mut().handle_input(keys::DOWN);
    let output = list_text(&selector, 120);
    assert!(output.contains("  ✓ medium"), "{output}");
    assert!(output.contains("→   high"), "{output}");
}

fn ids(selector: &SettingsSelector) -> Vec<String> {
    selector.settings_list().borrow().items().iter().map(|item| item.id.clone()).collect()
}

#[test]
fn orders_settings_like_upstream_with_and_without_image_support() {
    let _kb = setup();
    let with_images =
        SettingsSelector::with_image_support(SettingsConfig::default(), SettingsCallbacks::default(), true);
    let ids = ids(&with_images);
    assert_eq!(
        ids[..13],
        [
            "autocompact",
            "show-images",
            "image-width-cells",
            "auto-resize-images",
            "block-images",
            "skill-commands",
            "show-hardware-cursor",
            "editor-padding",
            "output-padding",
            "autocomplete-max-visible",
            "clear-on-shrink",
            "terminal-progress",
            "steering-mode",
        ]
    );
    assert_eq!(ids.len(), 34);
    assert_eq!(ids.last().map(String::as_str), Some("theme"));
    let without = SettingsSelector::with_image_support(SettingsConfig::default(), SettingsCallbacks::default(), false);
    let without = self::ids(&without);
    assert_eq!(without[..2], ["autocompact", "auto-resize-images"]);
    assert_eq!(without.len(), 32);
}

#[test]
fn renders_between_borders_and_navigates_with_a_terminal() {
    let cancelled = Calls::new();
    let on_cancel = cancelled.counter();
    let harness = Harness::new(100, 30, move |_| {
        let callbacks = SettingsCallbacks { on_cancel: Box::new(on_cancel), ..SettingsCallbacks::default() };
        SettingsSelector::with_image_support(SettingsConfig::default(), callbacks, false)
    });
    let viewport = harness.viewport();
    assert!(viewport[0].starts_with("────"), "{viewport:?}");
    let screen = harness.screen();
    assert!(screen.contains("→ Auto-compact"), "{screen}");
    assert!(screen.contains("Automatically compact context when it gets too large"), "{screen}");
    assert!(screen.contains("Type to search · Enter/Space to change · Esc to cancel"), "{screen}");
    assert!(screen.contains("(1/32)"), "{screen}");
    harness.send(keys::DOWN);
    let screen = harness.screen();
    assert!(screen.contains("→ Auto-resize images"), "{screen}");
    assert!(screen.contains("Resize large images to 2000x2000 max"), "{screen}");
    harness.send(keys::UP);
    harness.send(keys::UP);
    assert!(harness.screen().contains("→ Theme"), "{}", harness.screen());
    harness.send(keys::ESCAPE);
    assert_eq!(cancelled.len(), 1);
}

#[test]
fn dispatches_parsed_values() {
    let _kb = setup();
    let steering = Calls::new();
    let http = Calls::new();
    let quiet = Calls::new();
    let trust = Calls::new();
    let pad = Calls::new();
    let auto_compact = Calls::new();
    let share = Calls::new();
    let callbacks = SettingsCallbacks {
        on_session_share_destination_change: Box::new(share.recorder()),
        on_steering_mode_change: Box::new(steering.recorder()),
        on_http_idle_timeout_ms_change: Box::new(http.recorder()),
        on_quiet_startup_change: Box::new(quiet.recorder()),
        on_default_project_trust_change: Box::new(trust.recorder()),
        on_output_pad_change: Box::new(pad.recorder()),
        on_auto_compact_change: Box::new(auto_compact.recorder()),
        ..SettingsCallbacks::default()
    };
    let mut selector = SettingsSelector::new(SettingsConfig::default(), callbacks);
    send(&mut selector, &[keys::SPACE]);
    let list = selector.settings_list();
    for (id, presses) in
        [("steering-mode", 2), ("http-idle-timeout", 2), ("quiet-startup", 3), ("share-destination", 3)]
    {
        list.borrow_mut().select_item(id);
        for _ in 0..presses {
            list.borrow_mut().handle_input(keys::ENTER);
        }
    }
    for (id, presses) in [("default-project-trust", 3), ("output-padding", 2)] {
        list.borrow_mut().select_item(id);
        for _ in 0..presses {
            list.borrow_mut().handle_input(keys::ENTER);
        }
    }
    assert_eq!(auto_compact.get(), [false]);
    assert_eq!(steering.get(), [QueueMode::All, QueueMode::OneAtATime]);
    assert_eq!(http.get(), [0, 30_000]);
    assert_eq!(quiet.get(), [QuietStartup::On, QuietStartup::Header, QuietStartup::Off]);
    let expected = [DefaultProjectTrust::Always, DefaultProjectTrust::Never, DefaultProjectTrust::Ask];
    assert_eq!(trust.get(), expected);
    assert_eq!(pad.get(), [0, 1]);
    use crate::settings::schema::SessionShareDestination::{Auto, Gist, Radius};
    assert_eq!(share.get(), [Gist, Radius, Auto]);
    assert_eq!(format_http_idle_timeout_ms(45_500), "45.5 sec");
    assert_eq!(format_http_idle_timeout_ms(90_000), "90 sec");
    let trust = list.borrow().items().iter().find(|item| item.id == "default-project-trust").cloned();
    assert_eq!(trust.map(|item| item.current_value).as_deref(), Some("Ask"));
}

#[test]
fn value_updater_reverts_a_rejected_change_from_a_callback() {
    let _kb = setup();
    type Updater = Box<dyn Fn(&str, &str)>;
    let updater: Rc<RefCell<Option<Updater>>> = Rc::default();
    let handle = updater.clone();
    let callbacks = SettingsCallbacks {
        on_tui_mode_change: Box::new(move |mode: TuiMode| {
            assert_eq!(mode, TuiMode::Regular);
            if let Some(update) = handle.borrow().as_ref() {
                update("tui-mode", "fullscreen");
            }
        }),
        ..SettingsCallbacks::default()
    };
    let mut selector = SettingsSelector::new(SettingsConfig::default(), callbacks);
    *updater.borrow_mut() = Some(Box::new(selector.value_updater()));
    selector.settings_list().borrow_mut().select_item("tui-mode");
    selector.handle_input(keys::ENTER);
    let current = selector.settings_list().borrow().items().iter().find(|item| item.id == "tui-mode").cloned();
    assert_eq!(current.map(|item| item.current_value).as_deref(), Some("fullscreen"));
}

#[test]
fn persisting_callbacks_write_settings_and_config_reads_them() {
    let _theme = crate::theme::tests::global_state();
    let _kb = setup();
    let settings = Arc::new(SettingsManager::in_memory(json!({
        "defaultProvider": "anthropic",
        "defaultModel": "claude",
        "fullscreenWheelScrollLines": 7,
        "quietStartup": "header",
        "defaultProjectTrust": "never",
        "sessionShareDestination": "radius",
        "warnings": {"anthropicExtraUsage": false},
    })));
    let config = SettingsConfig::from_settings(&settings);
    assert_eq!(config.default_model, "anthropic/claude");
    assert_eq!(config.fullscreen_wheel_scroll_lines, Some(7));
    assert_eq!(config.quiet_startup, QuietStartup::Header);
    assert_eq!(config.default_project_trust, DefaultProjectTrust::Never);
    assert_eq!(config.session_share_destination, crate::settings::schema::SessionShareDestination::Radius);
    // The removed warnings setting stays in the file as an unknown key.
    assert!(settings.settings().extra.contains_key("warnings"));
    assert!(config.available_themes.iter().any(|theme| theme == "dark"));

    let mut selector = SettingsSelector::new(config, SettingsCallbacks::persisting(settings.clone()));
    let list = selector.settings_list();
    for id in [
        "fullscreen-wheel-scroll-lines",
        "default-project-trust",
        "hide-thinking",
        "editor-padding",
        "share-destination",
    ] {
        list.borrow_mut().select_item(id);
        selector.handle_input(keys::ENTER);
    }
    assert_eq!(settings.fullscreen_wheel_scroll_lines(), Some(10));
    assert_eq!(settings.default_project_trust(), "ask");
    assert!(settings.hide_thinking_block());
    assert_eq!(settings.editor_padding_x(), 1);
    assert_eq!(settings.session_share_destination(), crate::settings::schema::SessionShareDestination::Auto);
    let invalid = SettingsManager::in_memory(json!({"sessionShareDestination": "dropbox"}));
    assert_eq!(invalid.session_share_destination(), crate::settings::schema::SessionShareDestination::Auto);
}
