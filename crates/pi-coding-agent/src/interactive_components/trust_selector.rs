//! [`TrustSelector`]: the `/trust` dialog (upstream `trust-selector.ts`).

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use pi_tui::{Component, Container, component, get_keybindings};

use super::common::{bold, border, fg, spacer, text};
use super::{key_hint, raw_key_hint};
use crate::resources::trust::{
    ProjectTrustOption, ProjectTrustStoreEntry, ProjectTrustUpdate, format_saved_decision, project_trust_options,
};
use crate::theme::ThemeColor;

/// The chosen option: the session trust and the `trust.json` changes for
/// [`crate::resources::trust::ProjectTrustStore::set_many`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustSelection {
    pub trusted: bool,
    pub updates: Vec<ProjectTrustUpdate>,
}

pub struct TrustSelectorOptions {
    pub cwd: std::path::PathBuf,
    pub saved_decision: Option<ProjectTrustStoreEntry>,
    pub project_trusted: bool,
    pub on_select: Box<dyn FnMut(TrustSelection)>,
    pub on_cancel: Box<dyn FnMut()>,
}

pub struct TrustSelector {
    container: Container,
    list: Rc<RefCell<Container>>,
    selected_index: usize,
    trust_options: Vec<ProjectTrustOption>,
    saved_decision: Option<ProjectTrustStoreEntry>,
    on_select: Box<dyn FnMut(TrustSelection)>,
    on_cancel: Box<dyn FnMut()>,
}

impl TrustSelector {
    pub fn new(options: TrustSelectorOptions) -> Self {
        let trust_options = project_trust_options(&options.cwd, false);
        let saved_decision = options.saved_decision;
        let selected_index =
            trust_options.iter().position(|option| is_saved_option(option, saved_decision.as_ref())).unwrap_or(0);

        let mut container = Container::new();
        container.add_child(border());
        container.add_child(spacer(1));
        container.add_child(text(&fg(ThemeColor::Accent, &bold("Project trust")), 1, 0));
        container.add_child(text(&fg(ThemeColor::Muted, &display(&options.cwd)), 1, 0));
        container.add_child(spacer(1));
        let trust_path = trust_options.first().and_then(|option| option.saved_path.as_deref());
        let saved = format_saved_decision(trust_path, saved_decision.as_ref());
        container.add_child(text(&fg(ThemeColor::Muted, &format!("Saved decision: {saved}")), 1, 0));
        let session = if options.project_trusted { "trusted" } else { "untrusted" };
        container.add_child(text(&fg(ThemeColor::Muted, &format!("Current session: {session}")), 1, 0));
        container.add_child(spacer(1));
        let list = component(Container::new());
        container.add_child(list.clone());
        container.add_child(spacer(1));
        let hint = format!(
            "{}  {}  {}",
            raw_key_hint("↑↓", "navigate"),
            key_hint("tui.select.confirm", "save"),
            key_hint("tui.select.cancel", "cancel")
        );
        container.add_child(text(&hint, 1, 0));
        container.add_child(spacer(1));
        container.add_child(border());

        let mut selector = Self {
            container,
            list,
            selected_index,
            trust_options,
            saved_decision,
            on_select: options.on_select,
            on_cancel: options.on_cancel,
        };
        selector.update_list();
        selector
    }

    fn update_list(&mut self) {
        let mut list = self.list.borrow_mut();
        list.clear();
        for (index, option) in self.trust_options.iter().enumerate() {
            let selected = index == self.selected_index;
            let marker = if is_saved_option(option, self.saved_decision.as_ref()) {
                fg(ThemeColor::Accent, "✓ ")
            } else {
                "  ".into()
            };
            let prefix = if selected { fg(ThemeColor::Accent, "→ ") } else { "  ".into() };
            let label = fg(if selected { ThemeColor::Accent } else { ThemeColor::Text }, &option.label);
            list.add_child(text(&format!("{prefix}{marker}{label}"), 1, 0));
        }
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn is_saved_option(option: &ProjectTrustOption, saved: Option<&ProjectTrustStoreEntry>) -> bool {
    match (&option.saved_path, saved) {
        (Some(path), Some(saved)) => saved.decision == option.trusted && saved.path == *path,
        _ => false,
    }
}

impl Component for TrustSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        if kb.matches(data, "tui.select.up") || data == "k" {
            self.selected_index = self.selected_index.saturating_sub(1);
            self.update_list();
        } else if kb.matches(data, "tui.select.down") || data == "j" {
            self.selected_index = (self.selected_index + 1).min(self.trust_options.len().saturating_sub(1));
            self.update_list();
        } else if kb.matches(data, "tui.select.confirm") || data == "\n" {
            if let Some(option) = self.trust_options.get(self.selected_index) {
                let selection = TrustSelection { trusted: option.trusted, updates: option.updates.clone() };
                (self.on_select)(selection);
            }
        } else if kb.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
