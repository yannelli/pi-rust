use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::SelectItem;
use pi_tui::components::settings_list::{SettingItem, SubmenuDone};
use serde_json::Value;

use super::layout_editor::LayoutEditor;
use super::{BuiltinStatusLine, preset_names, theme_names};
use crate::interactive_components::settings_selector::{SelectSubmenu, SelectSubmenuOptions};
use crate::settings::schema::StatusLineSettings;

pub(super) fn choice(id: &str, label: &str, description: &str, current: &str, values: &[&str]) -> SettingItem {
    let options: Vec<SelectItem> = values
        .iter()
        .map(|value| SelectItem {
            value: (*value).into(),
            label: match *value {
                "" => "none",
                " " => "1 space",
                "  " => "2 spaces",
                value => value,
            }
            .into(),
            description: None,
        })
        .collect();
    let title = label.to_string();
    let description = description.to_string();
    SettingItem {
        id: id.into(),
        label: label.into(),
        description: Some(description.clone()),
        current_value: current.into(),
        submenu: Some(Rc::new(move |current, done: SubmenuDone| {
            let cancel = done.clone();
            Rc::new(RefCell::new(SelectSubmenu::new(
                &title,
                &description,
                options.clone(),
                current,
                move |value| done.done(Some(value)),
                move || cancel.done(None),
                None,
                SelectSubmenuOptions { searchable: true, ..Default::default() },
            )))
        })),
        ..Default::default()
    }
}

pub(super) fn build_items(value: &Value) -> Vec<SettingItem> {
    let field = |key, fallback| value.get(key).and_then(Value::as_str).unwrap_or(fallback);
    let settings: StatusLineSettings = serde_json::from_value(value.clone()).unwrap_or_default();
    let builtin = BuiltinStatusLine::from_settings(&settings, 0);
    let lines = super::layout_editor::layout_values(&builtin);
    let themes: Vec<&str> = std::iter::once("preset").chain(std::iter::once("terminal")).chain(theme_names()).collect();
    let presets: Vec<&str> = preset_names().collect();
    let context: Vec<_> = builtin
        .lines
        .iter()
        .flatten()
        .filter_map(|item| match item.kind {
            super::segments::ItemKind::Segment(segment) if super::editor::is_context(segment) => Some(segment.name()),
            _ => None,
        })
        .collect();
    let context = match context.as_slice() {
        [] => "hidden",
        [name] => name,
        _ => "custom",
    };
    let source = match field("type", "builtin") {
        "claude" => "from-claude",
        "cursor" => "from-cursor",
        "codex" => "from-codex",
        source => source,
    };
    vec![
        choice(
            "source",
            "Source",
            "Appearance edits switch to builtin. Existing command settings are kept.",
            source,
            &["builtin", "off", "command", "from-claude", "from-cursor", "from-codex"],
        ),
        choice(
            "preset",
            "Preset",
            "Resets layout, shape, theme, icons, and separator overrides.",
            field("preset", "minimal"),
            &presets,
        ),
        choice(
            "theme",
            "Palette",
            "Colors for the status line, independent of the harness theme.",
            field("theme", "preset"),
            &themes,
        ),
        choice(
            "chip",
            "Pill shape",
            "Rounded, slanted and powerline shapes need a Nerd Font.",
            field("chip", field("style", "preset")),
            &["preset", "plain", "block", "rounded", "powerline", "slanted", "brackets", "dot"],
        ),
        choice(
            "icons",
            "Icons",
            "Choose icons supported by your terminal font.",
            field("icons", "preset"),
            &["preset", "none", "unicode", "nerd", "emoji"],
        ),
        choice(
            "layout",
            "Alignment",
            "Spread honors fill; compact removes flexible gaps; stacked breaks at fill.",
            field("layout", "spread"),
            &["spread", "compact", "stacked"],
        ),
        choice(
            "separator",
            "Separator",
            "Spacing between pills. Block shapes already include inner padding.",
            field("separator", "preset"),
            &["preset", "", " ", "  ", " · ", " | "],
        ),
        choice(
            "padding",
            "Left padding",
            "Spaces before the status line.",
            &settings.padding.unwrap_or(0.0).to_string(),
            &["0", "1", "2", "3"],
        ),
        choice(
            "context",
            "Context display",
            "Replaces context modules. Headroom is context capacity, not subscription quota.",
            context,
            &[
                "context",
                "contextUsed",
                "contextRemaining",
                "contextWindow",
                "contextBar",
                "contextTokens",
                "contextHeadroom",
                "hidden",
            ],
        ),
        SettingItem {
            id: "segments".into(),
            label: "Modules / order".into(),
            current_value: "edit".into(),
            description: Some("Add, remove, reorder, or move modules between rows. Apply to save.".into()),
            submenu: Some(Rc::new(move |_, done| Rc::new(RefCell::new(LayoutEditor::new(lines.clone(), done))))),
            ..Default::default()
        },
    ]
}
