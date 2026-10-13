//! [`ConfigSelector`]: the `pi config` resource toggles (upstream `config-selector.ts`), and
//! the runner that shows it on the terminal (upstream `cli/config-selector.ts`).

mod resource_list;
mod runner;

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::utils::{truncate_to_width_with, visible_width};
use pi_tui::{Component, Container};

pub use runner::{config_selector_fn, run_config_selector};

use super::common::{bold, border, fg, spacer};
use super::{key_hint, raw_key_hint};
use crate::packages::{ConfigEditor, ConfigWriteScope};
use crate::resources::brand::CONFIG_DIR_NAME;
use crate::theme::ThemeColor;
use resource_list::{ListAction, ResourceList};

/// Title, key hints, and the settings file being edited.
struct ConfigSelectorHeader {
    write_scope: ConfigWriteScope,
    project_mode_available: bool,
}

impl Component for ConfigSelectorHeader {
    fn render(&mut self, width: usize) -> Vec<String> {
        let project = self.write_scope == ConfigWriteScope::Project;
        let title = bold(if project { "Project Local Resources" } else { "Global Resources" });
        let sep = fg(ThemeColor::Muted, " · ");
        let switch_hint =
            if self.project_mode_available { key_hint("tui.input.tab", "switch mode") + &sep } else { String::new() };
        let action_hint =
            if project { raw_key_hint("space", "cycle inherit/+/-") } else { raw_key_hint("space", "toggle") };
        let hint = format!("{switch_hint}{action_hint}{sep}{}", raw_key_hint("esc", "close"));
        let spacing = width.saturating_sub(visible_width(&title) + visible_width(&hint)).max(1);
        let scope_hint = if project {
            fg(ThemeColor::Muted, &format!("{CONFIG_DIR_NAME}/settings.json · inherited global resources are dimmed"))
        } else {
            fg(ThemeColor::Muted, &format!("~/{CONFIG_DIR_NAME}/agent/settings.json"))
        };
        vec![
            truncate_to_width_with(&format!("{title}{}{hint}", " ".repeat(spacing)), width, "", false),
            truncate_to_width_with(&scope_hint, width, "", false),
        ]
    }
}

/// Lists resources by package or settings location and toggles them through a
/// [`ConfigEditor`]. Space or Enter toggles, Tab switches between global and project mode
/// when available, typing filters, Esc closes.
pub struct ConfigSelector {
    container: Container,
    header: Rc<RefCell<ConfigSelectorHeader>>,
    resource_list: Rc<RefCell<ResourceList>>,
    write_scope: ConfigWriteScope,
    project_mode_available: bool,
    on_close: Box<dyn FnMut()>,
    on_exit: Box<dyn FnMut()>,
    request_render: Box<dyn FnMut()>,
}

impl ConfigSelector {
    /// Starts in the editor's write scope. `on_close` runs on Esc; `on_exit` on a Ctrl+C that
    /// `tui.select.cancel` does not claim; `request_render` after toggles and mode switches.
    pub fn new(
        editor: ConfigEditor,
        on_close: impl FnMut() + 'static,
        on_exit: impl FnMut() + 'static,
        request_render: impl FnMut() + 'static,
        terminal_height: Option<usize>,
        project_mode_available: bool,
    ) -> Self {
        let write_scope = editor.write_scope();
        let header = Rc::new(RefCell::new(ConfigSelectorHeader { write_scope, project_mode_available }));
        let resource_list = Rc::new(RefCell::new(ResourceList::new(editor, terminal_height)));
        let mut container = Container::new();
        container.add_child(spacer(1));
        container.add_child(border());
        container.add_child(spacer(1));
        container.add_child(header.clone());
        container.add_child(spacer(1));
        container.add_child(resource_list.clone());
        container.add_child(spacer(1));
        container.add_child(border());
        Self {
            container,
            header,
            resource_list,
            write_scope,
            project_mode_available,
            on_close: Box::new(on_close),
            on_exit: Box::new(on_exit),
            request_render: Box::new(request_render),
        }
    }

    pub fn write_scope(&self) -> ConfigWriteScope {
        self.write_scope
    }

    /// Runs `f` with the editor, for example to read the toggled state.
    pub fn with_editor<T>(&self, f: impl FnOnce(&ConfigEditor) -> T) -> T {
        f(self.resource_list.borrow().editor())
    }

    fn switch_write_scope(&mut self) {
        self.write_scope = match self.write_scope {
            ConfigWriteScope::Global => ConfigWriteScope::Project,
            ConfigWriteScope::Project => ConfigWriteScope::Global,
        };
        self.header.borrow_mut().write_scope = self.write_scope;
        self.resource_list.borrow_mut().set_write_scope(self.write_scope);
    }
}

impl Component for ConfigSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let action = self.resource_list.borrow_mut().handle_key(data);
        match action {
            Some(ListAction::Cancel) => (self.on_close)(),
            Some(ListAction::Exit) => (self.on_exit)(),
            Some(ListAction::Toggled) => (self.request_render)(),
            Some(ListAction::SwitchMode) if self.project_mode_available => {
                self.switch_write_scope();
                (self.request_render)();
            }
            Some(ListAction::SwitchMode) | None => {}
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.resource_list.borrow_mut().set_focused(focused);
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
