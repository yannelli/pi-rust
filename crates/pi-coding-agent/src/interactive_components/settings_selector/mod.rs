//! [`SettingsSelector`]: the `/settings` dialog (upstream `settings-selector.ts`), with its
//! submenus (upstream `settings-submenu.ts`).

mod config;
mod items;
mod persist;
mod providers;
mod retry;
mod submenu;
mod submenus;
mod theme_submenu;

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::SettingsList;
use pi_tui::components::settings_list::{SettingsListOptions, SubmenuDone, SubmenuFactory};
use pi_tui::terminal_image::get_capabilities;
use pi_tui::{Component, ComponentRef, Container, MouseResponse, TuiMouseEvent};

pub use config::{
    DefaultProjectTrust, HTTP_IDLE_TIMEOUT_CHOICES, OnChange, OnModel, OnModelLevel, OnText, SettingsCallbacks,
    SettingsConfig, format_http_idle_timeout_ms,
};
pub use submenu::{
    SelectSubmenu, SelectSubmenuOptions, StepContext, StepOptionsFn, StepPreselectFn, StepText, SteppedSubmenu,
    SteppedSubmenuOptions, SteppedSubmenuStep, submenu_layout,
};

use super::common::border;
use crate::theme::get_settings_list_theme;
use items::{Submenus, build_items, dispatch_change};
use submenus::{ModelThinkingData, model_setting_key, model_thinking_factory, overrides_summary};
use theme_submenu::{PreviewFn, ThemeSubmenu};

type SharedCallbacks = Rc<RefCell<SettingsCallbacks>>;
type ValueUpdates = Rc<RefCell<Vec<(String, String)>>>;

/// The settings dialog: a searchable [`SettingsList`] between two borders. Each change calls
/// the matching [`SettingsCallbacks`] entry; Esc calls `on_cancel`.
pub struct SettingsSelector {
    container: Container,
    settings_list: Rc<RefCell<SettingsList>>,
    updates: ValueUpdates,
}

impl SettingsSelector {
    /// Shows the image settings when the terminal supports inline images.
    pub fn new(config: SettingsConfig, callbacks: SettingsCallbacks) -> Self {
        Self::with_image_support(config, callbacks, get_capabilities().images.is_some())
    }

    /// Like [`Self::new`], with image support given instead of detected.
    pub fn with_image_support(config: SettingsConfig, callbacks: SettingsCallbacks, supports_images: bool) -> Self {
        let callbacks: SharedCallbacks = Rc::new(RefCell::new(callbacks));
        let overrides = Rc::new(RefCell::new(config.model_thinking_levels.clone()));
        let current_default_model_key = config
            .available_default_models
            .iter()
            .any(|model| model_setting_key(model) == config.default_model)
            .then(|| config.default_model.clone());
        let thinking = Rc::new(ModelThinkingData {
            models: config.available_default_models.clone(),
            current_model_key: config.current_model.as_ref().map(model_setting_key),
            current_default_model_key,
            global_level: config.thinking_level,
            overrides: overrides.clone(),
        });
        let submenus = Submenus {
            model_thinking: model_thinking_factory(thinking, callbacks.clone()),
            model_thinking_summary: overrides_summary(&overrides.borrow()),
            theme: theme_factory(&config, &callbacks),
            providers: providers::factory(&config, callbacks.clone()),
        };
        let items = build_items(&config, supports_images, submenus);

        let (on_change, on_cancel) = (callbacks.clone(), callbacks);
        let settings_list = Rc::new(RefCell::new(SettingsList::new(
            items,
            10,
            get_settings_list_theme(),
            move |id: &str, value: &str| dispatch_change(&mut on_change.borrow_mut(), id, value),
            move || (on_cancel.borrow_mut().on_cancel)(),
            SettingsListOptions { enable_search: true },
        )));
        let mut container = Container::new();
        container.add_child(border());
        container.add_child(settings_list.clone());
        container.add_child(border());
        Self { container, settings_list, updates: Rc::default() }
    }

    /// The list of settings (upstream `getSettingsList`). Do not borrow it from a callback.
    pub fn settings_list(&self) -> Rc<RefCell<SettingsList>> {
        self.settings_list.clone()
    }

    /// Sets the value shown for a setting, for example to revert a change the host rejected.
    /// Safe to call from callbacks: the update applies before the next render or input.
    pub fn value_updater(&self) -> impl Fn(&str, &str) + 'static {
        let updates = self.updates.clone();
        move |id: &str, value: &str| updates.borrow_mut().push((id.to_string(), value.to_string()))
    }

    fn apply_updates(&mut self) {
        let updates = std::mem::take(&mut *self.updates.borrow_mut());
        let mut list = self.settings_list.borrow_mut();
        for (id, value) in updates {
            list.update_value(&id, &value);
        }
    }
}

/// The theme submenu, previewing through `on_theme_preview`.
fn theme_factory(config: &SettingsConfig, callbacks: &SharedCallbacks) -> SubmenuFactory {
    let preview: PreviewFn = {
        let callbacks = callbacks.clone();
        Rc::new(move |setting: &str| {
            if let Some(preview) = callbacks.borrow_mut().on_theme_preview.as_mut() {
                preview(setting);
            }
        })
    };
    let (terminal_theme, available) = (config.terminal_theme, config.available_themes.clone());
    Rc::new(move |current: &str, done: SubmenuDone| -> ComponentRef {
        let submenu = ThemeSubmenu::new(current, terminal_theme, available.clone(), preview.clone(), done);
        Rc::new(RefCell::new(submenu))
    })
}

impl Component for SettingsSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.apply_updates();
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.apply_updates();
        self.settings_list.borrow_mut().handle_input(data);
        self.apply_updates();
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn captures_viewport_input(&self) -> bool {
        self.settings_list.borrow().captures_viewport_input()
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let response = self.container.handle_mouse(event);
        self.apply_updates();
        response
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
