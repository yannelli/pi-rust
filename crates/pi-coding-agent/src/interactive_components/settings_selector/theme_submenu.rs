//! The theme submenu of the settings selector: one theme, or separate light and dark themes
//! that follow the terminal appearance (upstream `ThemeSubmenu`).

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::settings_list::{SettingItem, SettingsListOptions, SubmenuDone, SubmenuFactory};
use pi_tui::components::{SelectItem, SettingsList};
use pi_tui::{Component, ComponentRef, Container, component};

use super::super::common::{bold, fg, spacer, text};
use super::submenu::{SelectSubmenu, SelectSubmenuOptions};
use crate::theme::{SYSTEM_THEME_NAME, TerminalTheme, ThemeColor, get_settings_list_theme, parse_auto_theme_setting};

/// Previews a theme setting (a name or a `light/dark` pair).
pub(super) type PreviewFn = Rc<dyn Fn(&str)>;

const AUTOMATIC_THEME_VALUE: &str = "/";

/// Theme items with a check mark on `current_theme`.
pub(super) fn theme_items(available_themes: &[String], current_theme: &str) -> Vec<SelectItem> {
    available_themes
        .iter()
        .map(|name| {
            let marker = if name == current_theme { "✓ " } else { "  " };
            let item = SelectItem::new(name, &format!("{marker}{name}"));
            if name == SYSTEM_THEME_NAME {
                item.with_description("Theme created from your terminal's colors")
            } else {
                item
            }
        })
        .collect()
}

/// The system theme comes first, then automatic mode, then the remaining themes.
fn single_mode_theme_items(available_themes: &[String], current_theme: &str) -> Vec<SelectItem> {
    let mut items = theme_items(available_themes, current_theme);
    let system = items.iter().position(|item| item.value == SYSTEM_THEME_NAME).map(|index| items.remove(index));
    let automatic = SelectItem::new(AUTOMATIC_THEME_VALUE, "  automatic")
        .with_description("Use separate themes for light and dark terminal appearance");
    system.into_iter().chain(std::iter::once(automatic)).chain(items).collect()
}

fn preferred_theme(available_themes: &[String], preferred: Option<&str>, fallback: &str) -> String {
    let available = |name: &str| available_themes.iter().any(|theme| theme == name);
    match preferred {
        Some(preferred) if available(preferred) => preferred.to_string(),
        _ if available(fallback) => fallback.to_string(),
        _ => available_themes.first().cloned().unwrap_or_else(|| fallback.to_string()),
    }
}

fn default_automatic_themes(current_setting: &str, available_themes: &[String]) -> (String, String) {
    if let Some(auto) = parse_auto_theme_setting(Some(current_setting)) {
        return (auto.light_theme, auto.dark_theme);
    }
    let fixed = (!current_setting.contains('/')).then_some(current_setting);
    let name = preferred_theme(available_themes, fixed, SYSTEM_THEME_NAME);
    (name.clone(), name)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeMode {
    Single,
    Automatic,
}

struct ThemeState {
    available_themes: Vec<String>,
    terminal_theme: TerminalTheme,
    original_setting: String,
    mode: ThemeMode,
    single_theme: String,
    light_theme: String,
    dark_theme: String,
}

impl ThemeState {
    fn theme_setting(&self) -> String {
        if self.mode == ThemeMode::Automatic { self.automatic_setting() } else { self.single_theme.clone() }
    }

    fn active_automatic_theme(&self) -> String {
        if self.terminal_theme == TerminalTheme::Light { self.light_theme.clone() } else { self.dark_theme.clone() }
    }

    fn automatic_setting(&self) -> String {
        format!("{}/{}", self.light_theme, self.dark_theme)
    }
}

enum ThemeEvent {
    ShowSingle,
    ShowAutomatic,
}

/// Shared handles that the submenu's callbacks use.
#[derive(Clone)]
struct Handles {
    state: Rc<RefCell<ThemeState>>,
    preview: PreviewFn,
    done: SubmenuDone,
    events: Rc<RefCell<Vec<ThemeEvent>>>,
}

impl Handles {
    fn preview_current(&self) {
        let setting = self.state.borrow().theme_setting();
        (self.preview)(&setting);
    }

    fn cancel(&self) {
        let original = self.state.borrow().original_setting.clone();
        (self.preview)(&original);
        self.done.done(None);
    }
}

pub(super) struct ThemeSubmenu {
    container: Container,
    input: Option<ComponentRef>,
    handles: Handles,
}

impl ThemeSubmenu {
    pub(super) fn new(
        current_setting: &str,
        terminal_theme: TerminalTheme,
        available_themes: Vec<String>,
        preview: PreviewFn,
        done: SubmenuDone,
    ) -> Self {
        let auto = parse_auto_theme_setting(Some(current_setting));
        let (light_theme, dark_theme) = default_automatic_themes(current_setting, &available_themes);
        let fixed = (auto.is_none() && !current_setting.contains('/')).then_some(current_setting);
        let mut state = ThemeState {
            available_themes,
            terminal_theme,
            original_setting: current_setting.to_string(),
            mode: if auto.is_some() { ThemeMode::Automatic } else { ThemeMode::Single },
            single_theme: String::new(),
            light_theme,
            dark_theme,
        };
        let active = auto.is_some().then(|| state.active_automatic_theme());
        state.single_theme = preferred_theme(&state.available_themes, fixed.or(active.as_deref()), SYSTEM_THEME_NAME);
        let automatic = state.mode == ThemeMode::Automatic;
        let state = Rc::new(RefCell::new(state));
        let handles = Handles { state, preview, done, events: Rc::default() };
        let mut submenu = Self { container: Container::new(), input: None, handles };
        if automatic {
            submenu.show_automatic_menu();
        } else {
            submenu.show_single_menu();
        }
        submenu
    }

    fn set_content(&mut self, render: ComponentRef, input: ComponentRef) {
        self.container.clear();
        self.container.add_child(render);
        self.input = Some(input);
    }

    fn show_single_menu(&mut self) {
        let (items, single) = {
            let mut state = self.handles.state.borrow_mut();
            state.mode = ThemeMode::Single;
            (single_mode_theme_items(&state.available_themes, &state.single_theme), state.single_theme.clone())
        };
        let (on_select, on_cancel, on_change) = (self.handles.clone(), self.handles.clone(), self.handles.clone());
        let menu = component(SelectSubmenu::new(
            "Theme",
            "Select a theme, or choose automatic to follow terminal appearance.",
            items,
            &single,
            move |value: &str| {
                if value == AUTOMATIC_THEME_VALUE {
                    on_select.state.borrow_mut().mode = ThemeMode::Automatic;
                    on_select.preview_current();
                    on_select.events.borrow_mut().push(ThemeEvent::ShowAutomatic);
                    return;
                }
                on_select.state.borrow_mut().single_theme = value.to_string();
                on_select.done.done(Some(value));
            },
            move || on_cancel.cancel(),
            Some(Box::new(move |value: &str| {
                let setting = if value == AUTOMATIC_THEME_VALUE {
                    on_change.state.borrow().automatic_setting()
                } else {
                    value.to_string()
                };
                (on_change.preview)(&setting);
            })),
            SelectSubmenuOptions::default(),
        ));
        self.set_content(menu.clone(), menu);
    }

    fn show_automatic_menu(&mut self) {
        let (light, dark) = {
            let mut state = self.handles.state.borrow_mut();
            state.mode = ThemeMode::Automatic;
            (state.light_theme.clone(), state.dark_theme.clone())
        };
        let mut content = Container::new();
        content.add_child(text(&bold(&fg(ThemeColor::Accent, "Automatic Theme")), 0, 0));
        content.add_child(spacer(1));
        let muted = |line: &str| text(&fg(ThemeColor::Muted, line), 0, 0);
        content.add_child(muted("Choose themes for terminal light and dark appearance."));
        content.add_child(muted("Light/dark detection requires terminal support."));
        content.add_child(spacer(1));

        let fixed = |id: &str, label: &str, description: &str, value: &str| SettingItem {
            id: id.into(),
            label: label.into(),
            description: Some(description.into()),
            current_value: value.into(),
            values: Some(vec![value.into()]),
            submenu: None,
        };
        let items = vec![
            SettingItem {
                id: "light-theme".into(),
                label: "Light theme".into(),
                description: Some("Theme to use in automatic mode when the terminal is light".into()),
                current_value: light,
                values: None,
                submenu: Some(self.theme_select_factory(TerminalTheme::Light)),
            },
            SettingItem {
                id: "dark-theme".into(),
                label: "Dark theme".into(),
                description: Some("Theme to use in automatic mode when the terminal is dark".into()),
                current_value: dark,
                values: None,
                submenu: Some(self.theme_select_factory(TerminalTheme::Dark)),
            },
            fixed("apply", "Apply", "Save and go back", "save and go back"),
            fixed("single-mode", "Change mode", "Switch to one theme for light and dark", "switch to single theme"),
        ];
        let (on_change, on_cancel) = (self.handles.clone(), self.handles.clone());
        let list = component(SettingsList::new(
            items.clone(),
            items.len().min(10),
            get_settings_list_theme(),
            move |id: &str, _value: &str| match id {
                "single-mode" => {
                    let single = {
                        let mut state = on_change.state.borrow_mut();
                        state.mode = ThemeMode::Single;
                        let active = state.active_automatic_theme();
                        state.single_theme = active.clone();
                        active
                    };
                    (on_change.preview)(&single);
                    on_change.events.borrow_mut().push(ThemeEvent::ShowSingle);
                }
                "apply" => {
                    let setting = on_change.state.borrow().automatic_setting();
                    on_change.done.done(Some(&setting));
                }
                _ => {}
            },
            move || on_cancel.cancel(),
            SettingsListOptions::default(),
        ));
        content.add_child(list.clone());
        self.set_content(component(content), list);
    }

    /// The submenu of the light or dark theme item.
    fn theme_select_factory(&self, appearance: TerminalTheme) -> SubmenuFactory {
        let handles = self.handles.clone();
        Rc::new(move |current_value: &str, done: SubmenuDone| -> ComponentRef {
            let (title, description) = match appearance {
                TerminalTheme::Light => ("Light Theme", "Select the theme to use for light terminal appearance"),
                TerminalTheme::Dark => ("Dark Theme", "Select the theme to use for dark terminal appearance"),
            };
            let items = theme_items(&handles.state.borrow().available_themes, current_value);
            let (on_select, on_cancel, on_change) = (handles.clone(), handles.clone(), handles.clone());
            let (select_done, cancel_done) = (done.clone(), done);
            component(SelectSubmenu::new(
                title,
                description,
                items,
                current_value,
                move |value: &str| {
                    {
                        let mut state = on_select.state.borrow_mut();
                        match appearance {
                            TerminalTheme::Light => state.light_theme = value.to_string(),
                            TerminalTheme::Dark => state.dark_theme = value.to_string(),
                        }
                    }
                    on_select.preview_current();
                    select_done.done(Some(value));
                },
                move || {
                    on_cancel.preview_current();
                    cancel_done.done(None);
                },
                Some(Box::new(move |value: &str| (on_change.preview)(value))),
                SelectSubmenuOptions::default(),
            ))
        })
    }

    fn process_events(&mut self) {
        loop {
            let event = {
                let mut events = self.handles.events.borrow_mut();
                if events.is_empty() {
                    return;
                }
                events.remove(0)
            };
            match event {
                ThemeEvent::ShowSingle => self.show_single_menu(),
                ThemeEvent::ShowAutomatic => self.show_automatic_menu(),
            }
        }
    }
}

impl Component for ThemeSubmenu {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        if let Some(input) = self.input.clone() {
            input.borrow_mut().handle_input(data);
        }
        self.process_events();
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
