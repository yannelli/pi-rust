//! The `/extensions` browser: Installed and Marketplace tabs with search, details, and actions.
//! Space toggles on the Installed tab while the search box is empty and types a space otherwise.

mod input;
mod render;
#[cfg(test)]
mod tests_input;
#[cfg(test)]
mod tests_render;

use pi_tui::Component;
use pi_tui::components::{Input, InputOptions};
use pi_tui::fuzzy::fuzzy_filter;

/// One extension as the browser shows it, installed or from the catalog.
#[derive(Clone, Debug)]
pub struct ExtensionRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: String,
    pub version: String,
    pub path: String,
    pub tags: Vec<String>,
    /// `None` when the extension has no enabled state, such as a catalog entry.
    pub enabled: Option<bool>,
    pub installed: bool,
    pub update_available: bool,
    /// Extra detail lines, such as commands and tools, shown as given.
    pub details: Vec<String>,
}

/// A request from the browser to the parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserAction {
    Close,
    Reload,
    Install(String),
    Update(String),
    Remove(String),
    Toggle(String),
}

const PACKAGE_ID_PREFIX: &str = "package:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BrowserTab {
    Installed,
    Marketplace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowAction {
    Install,
    Update,
    Enable,
    Disable,
    Remove,
}

impl RowAction {
    pub(super) fn label(self) -> &'static str {
        match self {
            RowAction::Install => "Install",
            RowAction::Update => "Update",
            RowAction::Enable => "Enable",
            RowAction::Disable => "Disable",
            RowAction::Remove => "Remove",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    List,
    Details { id: String, action: usize },
    ConfirmRemove { id: String, name: String },
}

pub struct ExtensionsBrowser {
    tab: BrowserTab,
    installed: Vec<ExtensionRow>,
    catalog: Vec<ExtensionRow>,
    catalog_loaded: bool,
    catalog_error: Option<String>,
    search: Input,
    selected: usize,
    mode: Mode,
    status: Option<String>,
    busy: bool,
    reload_offered: bool,
    on_action: Box<dyn FnMut(BrowserAction)>,
}

impl ExtensionsBrowser {
    pub fn new(installed: Vec<ExtensionRow>, on_action: impl FnMut(BrowserAction) + 'static) -> Self {
        let options = InputOptions { placeholder: Some("type to search".to_string()), ..InputOptions::default() };
        Self {
            tab: BrowserTab::Installed,
            installed,
            catalog: Vec::new(),
            catalog_loaded: false,
            catalog_error: None,
            search: Input::new(options),
            selected: 0,
            mode: Mode::List,
            status: None,
            busy: false,
            reload_offered: false,
            on_action: Box::new(on_action),
        }
    }

    pub fn set_installed(&mut self, rows: Vec<ExtensionRow>) {
        self.installed = rows;
        self.after_rows_changed();
    }

    /// Replaces the catalog. `error` is shown inline on the Marketplace tab.
    pub fn set_catalog(&mut self, rows: Vec<ExtensionRow>, error: Option<String>) {
        self.catalog = rows;
        self.catalog_error = error;
        self.catalog_loaded = true;
        self.after_rows_changed();
    }

    /// While `busy`, mutations and closing are disabled.
    pub fn set_status(&mut self, message: String, busy: bool) {
        self.status = (!message.is_empty()).then_some(message);
        self.busy = busy;
    }

    pub fn offer_reload(&mut self) {
        self.reload_offered = true;
    }

    pub(super) fn rows(&self) -> &[ExtensionRow] {
        match self.tab {
            BrowserTab::Installed => &self.installed,
            BrowserTab::Marketplace => &self.catalog,
        }
    }

    /// Indexes into [`Self::rows`] that match the search query, best first.
    pub(super) fn visible(&self) -> Vec<usize> {
        let items: Vec<(usize, String)> =
            self.rows().iter().enumerate().map(|(index, row)| (index, search_text(row))).collect();
        fuzzy_filter(items, self.search.get_value(), |item| item.1.as_str()).into_iter().map(|item| item.0).collect()
    }

    pub(super) fn selected_row(&self) -> Option<&ExtensionRow> {
        let visible = self.visible();
        visible.get(self.selected).and_then(|index| self.rows().get(*index))
    }

    pub(super) fn row_by_id(&self, id: &str) -> Option<&ExtensionRow> {
        self.rows().iter().find(|row| row.id == id)
    }

    /// Package rows (`package:` ids) update and remove; extension rows only toggle.
    pub(super) fn row_actions(&self, row: &ExtensionRow) -> Vec<RowAction> {
        let mut actions = Vec::new();
        match row.enabled {
            Some(true) => actions.push(RowAction::Disable),
            Some(false) => actions.push(RowAction::Enable),
            None => {}
        }
        let managed = match self.tab {
            BrowserTab::Installed => row.id.starts_with(PACKAGE_ID_PREFIX),
            BrowserTab::Marketplace => row.installed,
        };
        if self.tab == BrowserTab::Marketplace && !row.installed {
            actions.push(RowAction::Install);
        }
        if managed {
            actions.push(RowAction::Update);
            actions.push(RowAction::Remove);
        }
        actions
    }

    fn after_rows_changed(&mut self) {
        let len = self.visible().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        let missing = match &self.mode {
            Mode::List => false,
            Mode::Details { id, .. } | Mode::ConfirmRemove { id, .. } => self.row_by_id(id).is_none(),
        };
        if missing {
            self.mode = Mode::List;
        }
        if let Mode::Details { id, action } = self.mode.clone() {
            let count = self.row_by_id(&id).map(|row| self.row_actions(row).len()).unwrap_or(0);
            self.mode = Mode::Details { id, action: action.min(count.saturating_sub(1)) };
        }
    }

    fn emit(&mut self, action: BrowserAction) {
        (self.on_action)(action);
    }

    /// Sets busy before the callback so keys queued behind it cannot start a second mutation.
    pub(super) fn emit_mutation(&mut self, action: BrowserAction, status: String) {
        self.set_status(status, true);
        self.emit(action);
    }

    /// Runs `action` on `row`. Remove asks for confirmation first; busy blocks every action.
    pub(super) fn request(&mut self, action: RowAction, row: &ExtensionRow) {
        if self.busy {
            return;
        }
        let id = row.id.clone();
        let name = row.name.clone();
        match action {
            RowAction::Install => self.emit_mutation(BrowserAction::Install(id), format!("Installing {name}…")),
            RowAction::Update => self.emit_mutation(BrowserAction::Update(id), format!("Updating {name}…")),
            RowAction::Enable => self.emit_mutation(BrowserAction::Toggle(id), format!("Enabling {name}…")),
            RowAction::Disable => self.emit_mutation(BrowserAction::Toggle(id), format!("Disabling {name}…")),
            RowAction::Remove => self.mode = Mode::ConfirmRemove { id, name },
        }
    }
}

fn search_text(row: &ExtensionRow) -> String {
    format!("{} {} {} {} {}", row.name, row.id, row.description, row.source, row.tags.join(" "))
}

impl Component for ExtensionsBrowser {
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
