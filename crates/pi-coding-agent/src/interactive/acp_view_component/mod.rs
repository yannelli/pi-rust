//! The `/acp` browser: Installed and Registry tabs with search, an action menu for installed
//! agents, and a confirmation before Remove.

mod input;
mod render;
#[cfg(test)]
mod tests;

use pi_tui::Component;
use pi_tui::components::{Input, InputOptions};
use pi_tui::fuzzy::fuzzy_filter;

use crate::acp_client::AcpScope;

/// One agent as the browser shows it: an `acpAgents` entry or a registry entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    /// The settings file of an installed agent; `None` for registry rows.
    pub scope: Option<AcpScope>,
    /// A global entry that a project entry with the same ID replaces.
    pub shadowed: bool,
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
    Install(String, AcpScope),
    Login(String),
    SetDefault(String),
    Toggle(String, AcpScope),
    Remove(String, AcpScope),
    /// Move the entry from this scope to the other one.
    Move(String, AcpScope),
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
    MoveToProject,
    MoveToGlobal,
    Remove,
}

impl MenuItem {
    pub(super) fn label(self) -> &'static str {
        match self {
            MenuItem::Login => "Log in",
            MenuItem::SetDefault => "Set as default",
            MenuItem::Enable => "Enable",
            MenuItem::Disable => "Disable",
            MenuItem::MoveToProject => "Move to project settings",
            MenuItem::MoveToGlobal => "Move to global settings",
            MenuItem::Remove => "Remove",
        }
    }
}

/// The install targets, in menu order.
pub(super) const INSTALL_SCOPES: [AcpScope; 2] = [AcpScope::Global, AcpScope::Project];

pub(super) fn scope_choice(scope: AcpScope) -> &'static str {
    match scope {
        AcpScope::Global => "Global settings: every project",
        AcpScope::Project => "Project settings: this project only",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    List,
    Menu { id: String, scope: AcpScope, item: usize },
    ConfirmRemove { id: String, scope: AcpScope, name: String },
    InstallScope { id: String, name: String, item: usize },
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

    pub(super) fn installed_row(&self, id: &str, scope: AcpScope) -> Option<&AcpRow> {
        self.installed.iter().find(|row| row.id == id && row.scope == Some(scope))
    }

    /// The actions of an installed row. A replaced global entry can only be toggled or removed.
    pub(super) fn menu_items(row: &AcpRow) -> Vec<MenuItem> {
        let toggle = if row.enabled == Some(false) { MenuItem::Enable } else { MenuItem::Disable };
        if row.shadowed {
            return vec![toggle, MenuItem::Remove];
        }
        let shift = match row.scope {
            Some(AcpScope::Project) => MenuItem::MoveToGlobal,
            _ => MenuItem::MoveToProject,
        };
        vec![MenuItem::Login, MenuItem::SetDefault, toggle, shift, MenuItem::Remove]
    }

    fn after_rows_changed(&mut self) {
        let len = self.visible().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        let missing = match &self.mode {
            Mode::List | Mode::InstallScope { .. } => false,
            Mode::Menu { id, scope, .. } | Mode::ConfirmRemove { id, scope, .. } => {
                self.installed_row(id, *scope).is_none()
            }
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

    /// Asks for the settings scope of a registry agent.
    pub(super) fn install(&mut self, row: &AcpRow) {
        if self.busy {
            return;
        }
        if row.installed {
            self.set_status(format!("{} is already installed.", row.name), false);
            return;
        }
        self.mode = Mode::InstallScope { id: row.id.clone(), name: row.name.clone(), item: 0 };
    }

    pub(super) fn install_in(&mut self, id: String, name: &str, scope: AcpScope) {
        self.mode = Mode::List;
        let status = format!("Installing {name} in the {} settings…", scope.label());
        self.emit_mutation(AcpAction::Install(id, scope), status);
    }

    /// Runs a menu item. Remove asks for confirmation first; busy blocks every item.
    pub(super) fn run(&mut self, item: MenuItem, row: &AcpRow) {
        if self.busy {
            return;
        }
        let id = row.id.clone();
        let name = row.name.clone();
        let Some(scope) = row.scope else { return };
        self.mode = Mode::List;
        match item {
            MenuItem::Login => self.emit(AcpAction::Login(id)),
            MenuItem::SetDefault => self.emit(AcpAction::SetDefault(id)),
            MenuItem::Enable => self.emit_mutation(AcpAction::Toggle(id, scope), format!("Enabling {name}…")),
            MenuItem::Disable => self.emit_mutation(AcpAction::Toggle(id, scope), format!("Disabling {name}…")),
            MenuItem::MoveToProject | MenuItem::MoveToGlobal => {
                self.emit_mutation(AcpAction::Move(id, scope), format!("Moving {name}…"))
            }
            MenuItem::Remove => self.mode = Mode::ConfirmRemove { id, scope, name },
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
