//! [`ScopedModelsSelector`]: enable, disable, and order the models that `app.model.cycle*`
//! cycles through (upstream `scoped-models-selector.ts`). Changes are session-only until saved.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pi_ai::Model;
use pi_tui::components::{Input, Text};
use pi_tui::keys::matches_key;
use pi_tui::{Component, Container, component, get_keybindings};

use super::common::{bold, border, fg, fuzzy_filter_by, spacer, text, visible_window};
use super::key_display_text;
use super::model_selector::model_search_text;
use crate::theme::{ThemeColor, theme};

/// `None` enables every model; `Some` is an explicit, ordered list of `provider/id`.
pub type EnabledIds = Option<Vec<String>>;

fn is_enabled(enabled: &EnabledIds, id: &str) -> bool {
    enabled.as_ref().is_none_or(|ids| ids.iter().any(|enabled| enabled == id))
}

fn normalize_enabled(result: Vec<String>, all_ids: &[String]) -> EnabledIds {
    let covers_all = result.len() == all_ids.len() && result.iter().all(|id| all_ids.contains(id));
    if covers_all { None } else { Some(result) }
}

fn toggle(enabled: &EnabledIds, all_ids: &[String], id: &str) -> EnabledIds {
    let Some(ids) = enabled else {
        return Some(all_ids.iter().filter(|model| *model != id).cloned().collect());
    };
    match ids.iter().position(|enabled| enabled == id) {
        Some(index) => {
            let mut ids = ids.clone();
            ids.remove(index);
            Some(ids)
        }
        None => normalize_enabled(ids.iter().cloned().chain([id.to_string()]).collect(), all_ids),
    }
}

fn enable_all(enabled: &EnabledIds, all_ids: &[String], targets: Option<&[String]>) -> EnabledIds {
    let ids = enabled.as_ref()?;
    let mut result = ids.clone();
    for id in targets.unwrap_or(all_ids) {
        if !result.contains(id) {
            result.push(id.clone());
        }
    }
    normalize_enabled(result, all_ids)
}

fn clear_all(enabled: &EnabledIds, all_ids: &[String], targets: Option<&[String]>) -> EnabledIds {
    match (enabled, targets) {
        (None, Some(targets)) => Some(all_ids.iter().filter(|id| !targets.contains(id)).cloned().collect()),
        (None, None) => Some(Vec::new()),
        (Some(ids), targets) => {
            let targets = targets.unwrap_or(ids);
            Some(ids.iter().filter(|id| !targets.contains(id)).cloned().collect())
        }
    }
}

fn sorted_ids(enabled: &EnabledIds, all_ids: &[String]) -> Vec<String> {
    match enabled {
        None => all_ids.to_vec(),
        Some(ids) => ids.iter().cloned().chain(all_ids.iter().filter(|id| !ids.contains(id)).cloned()).collect(),
    }
}

#[derive(Clone)]
struct ModelItem {
    full_id: String,
    model: Option<Model>,
    enabled: bool,
}

/// What the selector edits.
pub struct ModelsConfig {
    pub all_models: Vec<Model>,
    pub enabled_model_ids: EnabledIds,
    /// Shown under the list, such as a catalog refresh status.
    pub refresh_status: Option<String>,
}

/// Session changes, saving, and cancel.
pub struct ModelsCallbacks {
    pub on_change: Box<dyn FnMut(EnabledIds)>,
    pub on_persist: Box<dyn FnMut(EnabledIds)>,
    pub on_cancel: Box<dyn FnMut()>,
}

/// Color of [`ScopedModelsSelector::set_refresh_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStatusKind {
    Muted,
    Success,
    Warning,
}

const MAX_VISIBLE: usize = 8;

pub struct ScopedModelsSelector {
    container: Container,
    search_input: Rc<RefCell<Input>>,
    list: Rc<RefCell<Container>>,
    footer_text: Rc<RefCell<Text>>,
    refresh_status_text: Option<Rc<RefCell<Text>>>,
    models_by_id: HashMap<String, Model>,
    all_ids: Vec<String>,
    enabled: EnabledIds,
    filtered: Vec<ModelItem>,
    selected_index: usize,
    is_dirty: bool,
    callbacks: ModelsCallbacks,
}

fn full_id(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

impl ScopedModelsSelector {
    pub fn new(config: ModelsConfig, callbacks: ModelsCallbacks) -> Self {
        let all_ids: Vec<String> = config.all_models.iter().map(full_id).collect();
        let models_by_id = config.all_models.into_iter().map(|model| (full_id(&model), model)).collect();
        let mut container = Container::new();
        container.add_child(border());
        container.add_child(spacer(1));
        container.add_child(text(&fg(ThemeColor::Accent, &bold("Model Configuration")), 0, 0));
        let session_hint = format!("Session-only. {} to save to settings.", key_display_text("app.models.save"));
        container.add_child(text(&fg(ThemeColor::Muted, &session_hint), 0, 0));
        container.add_child(spacer(1));
        let search_input = component(Input::default());
        container.add_child(search_input.clone());
        container.add_child(spacer(1));
        let list = component(Container::new());
        container.add_child(list.clone());
        container.add_child(spacer(1));
        let refresh_status_text = config.refresh_status.as_deref().filter(|status| !status.is_empty()).map(|status| {
            let status_text = component(Text::new(&fg(ThemeColor::Muted, &format!("  {status}")), 0, 0));
            container.add_child(status_text.clone());
            status_text
        });
        let footer_text = component(Text::new("", 0, 0));
        container.add_child(footer_text.clone());
        container.add_child(border());

        let mut selector = Self {
            container,
            search_input,
            list,
            footer_text,
            refresh_status_text,
            models_by_id,
            all_ids,
            enabled: config.enabled_model_ids,
            filtered: Vec::new(),
            selected_index: 0,
            is_dirty: false,
            callbacks,
        };
        selector.filtered = selector.build_items();
        selector.footer_text.borrow_mut().set_text(&selector.footer());
        selector.update_list();
        selector
    }

    /// Replaces the model list (after a catalog refresh), keeping the selected row.
    pub fn update_models(&mut self, models: Vec<Model>, enabled_model_ids: Option<EnabledIds>) {
        let selected = self.filtered.get(self.selected_index).map(|item| item.full_id.clone());
        if let Some(enabled) = enabled_model_ids {
            self.enabled = enabled;
        }
        self.all_ids = models.iter().map(full_id).collect();
        self.models_by_id = models.into_iter().map(|model| (full_id(&model), model)).collect();
        self.refresh();
        if let Some(index) = selected.and_then(|id| self.filtered.iter().position(|item| item.full_id == id)) {
            self.selected_index = index;
            self.update_list();
        }
    }

    pub fn set_refresh_status(&mut self, message: &str, kind: RefreshStatusKind) {
        let color = match kind {
            RefreshStatusKind::Muted => ThemeColor::Muted,
            RefreshStatusKind::Success => ThemeColor::Success,
            RefreshStatusKind::Warning => ThemeColor::Warning,
        };
        if let Some(status) = &self.refresh_status_text {
            status.borrow_mut().set_text(&fg(color, &format!("  {message}")));
        }
    }

    pub fn search_input(&self) -> Rc<RefCell<Input>> {
        self.search_input.clone()
    }

    fn build_items(&self) -> Vec<ModelItem> {
        sorted_ids(&self.enabled, &self.all_ids)
            .into_iter()
            .map(|id| ModelItem {
                model: self.models_by_id.get(&id).cloned(),
                enabled: is_enabled(&self.enabled, &id),
                full_id: id,
            })
            .collect()
    }

    fn footer(&self) -> String {
        let count = match &self.enabled {
            None => "all enabled".to_string(),
            Some(ids) => {
                let available = ids.iter().filter(|id| self.models_by_id.contains_key(*id)).count();
                let unavailable = ids.len() - available;
                let suffix = if unavailable > 0 { format!(" · {unavailable} unavailable") } else { String::new() };
                format!("{available}/{} enabled{suffix}", self.all_ids.len())
            }
        };
        let parts = [
            format!("{} toggle", key_display_text("tui.select.confirm")),
            format!("{} all", key_display_text("app.models.enableAll")),
            format!("{} clear", key_display_text("app.models.clearAll")),
            format!("{} provider", key_display_text("app.models.toggleProvider")),
            format!(
                "{}/{} reorder",
                key_display_text("app.models.reorderUp"),
                key_display_text("app.models.reorderDown")
            ),
            format!("{} save", key_display_text("app.models.save")),
            count,
        ];
        let line = format!("  {}", parts.join(" · "));
        if self.is_dirty {
            fg(ThemeColor::Dim, &format!("{line} ")) + &fg(ThemeColor::Warning, "(unsaved)")
        } else {
            fg(ThemeColor::Dim, &line)
        }
    }

    fn refresh(&mut self) {
        let query = self.search_input.borrow().get_value().to_string();
        let items = self.build_items();
        self.filtered = if query.is_empty() {
            items
        } else {
            fuzzy_filter_by(&items, &query, |item| {
                item.model.as_ref().map_or_else(|| item.full_id.clone(), model_search_text)
            })
        };
        self.selected_index = self.selected_index.min(self.filtered.len().saturating_sub(1));
        self.update_list();
        self.footer_text.borrow_mut().set_text(&self.footer());
    }

    fn notify_change(&mut self) {
        let enabled = self.enabled.clone();
        (self.callbacks.on_change)(enabled);
    }

    fn update_list(&mut self) {
        let mut list = self.list.borrow_mut();
        list.clear();
        if self.filtered.is_empty() {
            list.add_child(text(&fg(ThemeColor::Muted, "  No matching models"), 0, 0));
            return;
        }
        let (start, end) = visible_window(self.selected_index, self.filtered.len(), MAX_VISIBLE);
        for (index, item) in self.filtered.iter().enumerate().take(end).skip(start) {
            let selected = index == self.selected_index;
            let prefix = if selected { fg(ThemeColor::Accent, "→ ") } else { "  ".into() };
            let id = item.model.as_ref().map_or(item.full_id.as_str(), |model| model.id.as_str());
            let styled = if item.model.is_some() { id.to_string() } else { theme().strikethrough(id) };
            let model_text = if selected { fg(ThemeColor::Accent, &styled) } else { styled };
            let badge =
                item.model.as_ref().map_or_else(|| " [unavailable]".to_string(), |m| format!(" [{}]", m.provider));
            let status =
                if item.model.is_some() && item.enabled { fg(ThemeColor::Accent, "✓ ") } else { "  ".into() };
            list.add_child(text(&format!("{prefix}{status}{model_text}{}", fg(ThemeColor::Muted, &badge)), 0, 0));
        }
        if start > 0 || end < self.filtered.len() {
            let info = format!("  ({}/{})", self.selected_index + 1, self.filtered.len());
            list.add_child(text(&fg(ThemeColor::Muted, &info), 0, 0));
        }
        let selected = &self.filtered[self.selected_index];
        list.add_child(spacer(1));
        let name =
            selected.model.as_ref().map_or_else(|| "Model unavailable".into(), |m| format!("Model Name: {}", m.name));
        list.add_child(text(&fg(ThemeColor::Muted, &format!("  {name}")), 0, 0));
    }

    fn search_targets(&self) -> Option<Vec<String>> {
        let searching = !self.search_input.borrow().get_value().is_empty();
        searching.then(|| self.filtered.iter().map(|item| item.full_id.clone()).collect())
    }

    fn apply(&mut self, enabled: EnabledIds) {
        self.enabled = enabled;
        self.is_dirty = true;
        self.refresh();
        self.notify_change();
    }

    fn reorder(&mut self, delta: i64) {
        let Some(ids) = &self.enabled else { return };
        let Some(item) = self.filtered.get(self.selected_index) else { return };
        if !is_enabled(&self.enabled, &item.full_id) {
            return;
        }
        let Some(current) = ids.iter().position(|id| *id == item.full_id) else { return };
        let target = current as i64 + delta;
        if target < 0 || target >= ids.len() as i64 {
            return;
        }
        let mut ids = ids.clone();
        ids.swap(current, target as usize);
        self.selected_index = (self.selected_index as i64 + delta).max(0) as usize;
        self.apply(Some(ids));
    }
}

impl Component for ScopedModelsSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        let len = self.filtered.len();
        if kb.matches(data, "tui.select.up") {
            if len > 0 {
                self.selected_index = if self.selected_index == 0 { len - 1 } else { self.selected_index - 1 };
                self.update_list();
            }
            return;
        }
        if kb.matches(data, "tui.select.down") {
            if len > 0 {
                self.selected_index = if self.selected_index + 1 == len { 0 } else { self.selected_index + 1 };
                self.update_list();
            }
            return;
        }
        let (up, down) = (kb.matches(data, "app.models.reorderUp"), kb.matches(data, "app.models.reorderDown"));
        if up || down {
            self.reorder(if up { -1 } else { 1 });
            return;
        }
        if kb.matches(data, "tui.select.confirm") {
            if let Some(item) = self.filtered.get(self.selected_index) {
                let enabled = toggle(&self.enabled, &self.all_ids, &item.full_id.clone());
                self.apply(enabled);
            }
            return;
        }
        if kb.matches(data, "app.models.enableAll") {
            let targets = self.search_targets();
            self.apply(enable_all(&self.enabled, &self.all_ids, targets.as_deref()));
            return;
        }
        if kb.matches(data, "app.models.clearAll") {
            let targets = self.search_targets();
            self.apply(clear_all(&self.enabled, &self.all_ids, targets.as_deref()));
            return;
        }
        if kb.matches(data, "app.models.toggleProvider") {
            let Some(provider) = self.filtered.get(self.selected_index).and_then(|item| item.model.as_ref()) else {
                return;
            };
            let provider = provider.provider.clone();
            let ids: Vec<String> = self
                .all_ids
                .iter()
                .filter(|id| self.models_by_id.get(*id).is_some_and(|model| model.provider == provider))
                .cloned()
                .collect();
            let all_enabled = ids.iter().all(|id| is_enabled(&self.enabled, id));
            let enabled = if all_enabled {
                clear_all(&self.enabled, &self.all_ids, Some(&ids))
            } else {
                enable_all(&self.enabled, &self.all_ids, Some(&ids))
            };
            self.apply(enabled);
            return;
        }
        if kb.matches(data, "app.models.save") {
            let enabled = self.enabled.clone();
            (self.callbacks.on_persist)(enabled);
            self.is_dirty = false;
            self.footer_text.borrow_mut().set_text(&self.footer());
            return;
        }
        if matches_key(data, "ctrl+c") {
            if self.search_input.borrow().get_value().is_empty() {
                (self.callbacks.on_cancel)();
            } else {
                self.search_input.borrow_mut().set_value("");
                self.refresh();
            }
            return;
        }
        if matches_key(data, "escape") {
            (self.callbacks.on_cancel)();
            return;
        }
        self.search_input.borrow_mut().handle_input(data);
        self.refresh();
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.borrow_mut().focused = focused;
    }

    fn invalidate(&mut self) {
        self.container.invalidate();
    }
}
