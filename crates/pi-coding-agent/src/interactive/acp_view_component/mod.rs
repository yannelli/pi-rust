//! The `/acp` browser: Installed and Registry tabs with search, an action menu for installed
//! agents, and a confirmation before Remove.

mod input;
mod render;
#[cfg(test)]
mod tests;

use pi_tui::Component;
use pi_tui::components::{Input, InputOptions};
use pi_tui::fuzzy::fuzzy_filter;

/// One agent as the browser shows it: an `acpAgents` entry or a registry entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    /// The settings scope of an installed agent; empty for registry rows.
    pub scope: String,
    /// `None` for registry rows.
    pub enabled: Option<bool>,
    pub installed: bool,
    pub is_default: bool,
    pub builtin: bool,
    pub details: Vec<String>,
}

/// A request from the browser to the parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpAction {
    Close,
    Install(String),
    Login(String),
    SetDefault(String),
    Toggle(String),
    Remove(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AcpTab {
    Installed,
    Registry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuItem {
    Login,
    SetDefault,
    Enable,
    Disable,
    Remove,
}

impl MenuItem {
    pub(super) fn label(self) -> &'static str {
        match self {
            MenuItem::Login => "Log in",
            MenuItem::SetDefault => "Set as default",
            MenuItem::Enable => "Enable",
            MenuItem::Disable => "Disable",
            MenuItem::Remove => "Remove",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    List,
    Menu { id: String, item: usize },
    ConfirmRemove { id: String, name: String },
}

pub struct AcpBrowser {
    tab: AcpTab,
    installed: Vec<AcpRow>,
    registry: Vec<AcpRow>,
    registry_loaded: bool,
    registry_error: Option<String>,
    search: Input,
    selected: usize,
    mode: Mode,
    status: Option<String>,
    busy: bool,
    on_action: Box<dyn FnMut(AcpAction)>,
}

impl AcpBrowser {
    pub fn new(installed: Vec<AcpRow>, on_action: impl FnMut(AcpAction) + 'static) -> Self {
        let options = InputOptions { placeholder: Some("type to search".to_string()), ..InputOptions::default() };
        Self {
            tab: AcpTab::Installed,
            installed,
            registry: Vec::new(),
            registry_loaded: false,
            registry_error: None,
            search: Input::new(options),
            selected: 0,
            mode: Mode::List,
            status: None,
            busy: false,
            on_action: Box::new(on_action),
        }
    }

    pub fn set_installed(&mut self, rows: Vec<AcpRow>) {
        self.installed = rows;
        self.after_rows_changed();
    }

    /// Replaces the registry rows. `error` is shown on the Registry tab above the rows.
    pub fn set_registry(&mut self, rows: Vec<AcpRow>, error: Option<String>) {
        self.registry = rows;
        self.registry_error = error;
        self.registry_loaded = true;
        self.after_rows_changed();
    }

    /// While `busy`, changes and closing are disabled.
    pub fn set_status(&mut self, message: String, busy: bool) {
        self.status = (!message.is_empty()).then_some(message);
        self.busy = busy;
    }

    pub(super) fn rows(&self) -> &[AcpRow] {
        match self.tab {
            AcpTab::Installed => &self.installed,
            AcpTab::Registry => &self.registry,
        }
    }

    /// Indexes into [`Self::rows`] that match the search query, best first.
    pub(super) fn visible(&self) -> Vec<usize> {
        let items: Vec<(usize, String)> = self
            .rows()
            .iter()
            .enumerate()
            .map(|(index, row)| (index, format!("{} {} {}", row.name, row.id, row.description)))
            .collect();
        fuzzy_filter(items, self.search.get_value(), |item| item.1.as_str()).into_iter().map(|item| item.0).collect()
    }

    pub(super) fn selected_row(&self) -> Option<&AcpRow> {
        self.visible().get(self.selected).and_then(|index| self.rows().get(*index))
    }

    pub(super) fn installed_row(&self, id: &str) -> Option<&AcpRow> {
        self.installed.iter().find(|row| row.id == id)
    }

    pub(super) fn menu_items(row: &AcpRow) -> Vec<MenuItem> {
        let toggle = if row.enabled == Some(false) { MenuItem::Enable } else { MenuItem::Disable };
        vec![MenuItem::Login, MenuItem::SetDefault, toggle, MenuItem::Remove]
    }

    fn after_rows_changed(&mut self) {
        let len = self.visible().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        let missing = match &self.mode {
            Mode::List => false,
            Mode::Menu { id, .. } | Mode::ConfirmRemove { id, .. } => self.installed_row(id).is_none(),
        };
        if missing {
            self.mode = Mode::List;
        }
    }

    pub(super) fn emit(&mut self, action: AcpAction) {
        (self.on_action)(action);
    }

    /// Sets busy before the callback so keys queued behind it cannot start a second change.
    pub(super) fn emit_mutation(&mut self, action: AcpAction, status: String) {
        self.set_status(status, true);
        self.emit(action);
    }

    pub(super) fn install(&mut self, row: &AcpRow) {
        if self.busy {
            return;
        }
        if row.installed {
            self.set_status(format!("{} is already installed.", row.name), false);
            return;
        }
        self.emit_mutation(AcpAction::Install(row.id.clone()), format!("Installing {}…", row.name));
    }

    /// Runs a menu item. Remove asks for confirmation first; busy blocks every item.
    pub(super) fn run(&mut self, item: MenuItem, row: &AcpRow) {
        if self.busy {
            return;
        }
        let id = row.id.clone();
        let name = row.name.clone();
        self.mode = Mode::List;
        match item {
            MenuItem::Login => self.emit(AcpAction::Login(id)),
            MenuItem::SetDefault => self.emit(AcpAction::SetDefault(id)),
            MenuItem::Enable => self.emit_mutation(AcpAction::Toggle(id), format!("Enabling {name}…")),
            MenuItem::Disable => self.emit_mutation(AcpAction::Toggle(id), format!("Disabling {name}…")),
            MenuItem::Remove => self.mode = Mode::ConfirmRemove { id, name },
        }
    }
}

impl Component for AcpBrowser {
    fn render(&mut self, width: usize) -> Vec<String> {
        render::render(self, width)
    }

    fn handle_input(&mut self, data: &str) {
        input::handle(self, data);
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.focused = focused;
    }
}
