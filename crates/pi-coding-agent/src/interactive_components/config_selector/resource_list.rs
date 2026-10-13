//! The searchable resource list of the config selector (upstream `ResourceList`).

use pi_tui::components::Input;
use pi_tui::utils::truncate_to_width_with;
use pi_tui::{Component, get_keybindings, matches_key};

use super::super::common::{bold, fg, visible_window};
use crate::packages::{ConfigEditor, ConfigItem, ConfigWriteScope, ProjectOverrideState};
use crate::resources::SourceScope;
use crate::theme::ThemeColor;

/// A row: a group header, a subgroup header, or an item, by index into the editor's groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlatEntry {
    Group(usize),
    Subgroup(usize, usize),
    Item(usize, usize, usize),
}

impl FlatEntry {
    fn is_item(self) -> bool {
        matches!(self, FlatEntry::Item(..))
    }
}

/// What a key did that the selector must handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListAction {
    Cancel,
    Exit,
    SwitchMode,
    Toggled,
}

pub(super) struct ResourceList {
    editor: ConfigEditor,
    flat_items: Vec<FlatEntry>,
    filtered_items: Vec<FlatEntry>,
    selected_index: usize,
    search_input: Input,
    max_visible: usize,
}

fn truncate(text: &str, width: usize, ellipsis: &str) -> String {
    truncate_to_width_with(text, width, ellipsis, false)
}

impl ResourceList {
    pub(super) fn new(editor: ConfigEditor, terminal_height: Option<usize>) -> Self {
        // Chrome: top spacer, top border, spacer, two header lines, spacer, bottom spacer, bottom border.
        let chrome = 8;
        let max_visible = terminal_height.unwrap_or(24).saturating_sub(chrome).max(5);
        let mut list = Self {
            editor,
            flat_items: Vec::new(),
            filtered_items: Vec::new(),
            selected_index: 0,
            search_input: Input::default(),
            max_visible,
        };
        list.build_flat_list();
        list.filtered_items = list.flat_items.clone();
        list
    }

    pub(super) fn editor(&self) -> &ConfigEditor {
        &self.editor
    }

    pub(super) fn set_write_scope(&mut self, scope: ConfigWriteScope) {
        self.editor.set_write_scope(scope);
        self.build_flat_list();
        let query = self.search_input.get_value().to_string();
        self.filter_items(&query);
    }

    fn project_mode(&self) -> bool {
        self.editor.write_scope() == ConfigWriteScope::Project
    }

    fn item(&self, group: usize, subgroup: usize, item: usize) -> &ConfigItem {
        &self.editor.groups()[group].subgroups[subgroup].items[item]
    }

    fn build_flat_list(&mut self) {
        self.flat_items.clear();
        for (g, group) in self.editor.groups().iter().enumerate() {
            self.flat_items.push(FlatEntry::Group(g));
            for (s, subgroup) in group.subgroups.iter().enumerate() {
                self.flat_items.push(FlatEntry::Subgroup(g, s));
                self.flat_items.extend((0..subgroup.items.len()).map(|i| FlatEntry::Item(g, s, i)));
            }
        }
        self.selected_index = self.flat_items.iter().position(|entry| entry.is_item()).unwrap_or(0);
    }

    fn find_next_item(&self, from: usize, direction: i64) -> usize {
        let mut index = from as i64 + direction;
        while index >= 0 && (index as usize) < self.filtered_items.len() {
            if self.filtered_items[index as usize].is_item() {
                return index as usize;
            }
            index += direction;
        }
        from
    }

    fn filter_items(&mut self, query: &str) {
        if query.trim().is_empty() {
            self.filtered_items = self.flat_items.clone();
            self.select_first_item();
            return;
        }
        let query = query.to_lowercase();
        let matches = |item: &ConfigItem| {
            item.display_name.to_lowercase().contains(&query)
                || item.kind.dir_name().contains(&query)
                || item.path.display().to_string().to_lowercase().contains(&query)
        };
        let item_matches = |g: usize, s: usize, i: usize| matches(self.item(g, s, i));
        let subgroup_matches =
            |g: usize, s: usize| (0..self.editor.groups()[g].subgroups[s].items.len()).any(|i| item_matches(g, s, i));
        let group_matches = |g: usize| (0..self.editor.groups()[g].subgroups.len()).any(|s| subgroup_matches(g, s));
        self.filtered_items = self
            .flat_items
            .iter()
            .copied()
            .filter(|entry| match *entry {
                FlatEntry::Group(g) => group_matches(g),
                FlatEntry::Subgroup(g, s) => subgroup_matches(g, s),
                FlatEntry::Item(g, s, i) => item_matches(g, s, i),
            })
            .collect();
        self.select_first_item();
    }

    fn select_first_item(&mut self) {
        self.selected_index = self.filtered_items.iter().position(|entry| entry.is_item()).unwrap_or(0);
    }

    /// Handles a key; returns what the selector must do in response.
    pub(super) fn handle_key(&mut self, data: &str) -> Option<ListAction> {
        let kb = get_keybindings();
        let len = self.filtered_items.len();
        if kb.matches(data, "tui.select.up") {
            self.selected_index = self.find_next_item(self.selected_index, -1);
        } else if kb.matches(data, "tui.select.down") {
            self.selected_index = self.find_next_item(self.selected_index, 1);
        } else if kb.matches(data, "tui.select.pageUp") {
            let mut target = self.selected_index.saturating_sub(self.max_visible);
            while target < len && !self.filtered_items[target].is_item() {
                target += 1;
            }
            if target < len {
                self.selected_index = target;
            }
        } else if kb.matches(data, "tui.select.pageDown") {
            let mut target = (len as i64 - 1).min((self.selected_index + self.max_visible) as i64);
            while target >= 0 && !self.filtered_items[target as usize].is_item() {
                target -= 1;
            }
            if target >= 0 {
                self.selected_index = target as usize;
            }
        } else if kb.matches(data, "tui.select.cancel") {
            return Some(ListAction::Cancel);
        } else if matches_key(data, "ctrl+c") {
            return Some(ListAction::Exit);
        } else if kb.matches(data, "tui.input.tab") {
            return Some(ListAction::SwitchMode);
        } else if data == " " || kb.matches(data, "tui.select.confirm") {
            if let Some(FlatEntry::Item(g, s, i)) = self.filtered_items.get(self.selected_index).copied()
                && self.editor.toggle(g, s, i).is_some()
            {
                return Some(ListAction::Toggled);
            }
        } else {
            self.search_input.handle_input(data);
            let query = self.search_input.get_value().to_string();
            self.filter_items(&query);
        }
        None
    }

    fn render_checkbox(&self, item: &ConfigItem) -> String {
        if self.project_mode() {
            return match self.editor.override_state(item) {
                ProjectOverrideState::Load => fg(ThemeColor::Success, "[+]"),
                ProjectOverrideState::Unload => fg(ThemeColor::Warning, "[-]"),
                ProjectOverrideState::Inherit => fg(ThemeColor::Dim, if item.enabled { "[x]" } else { "[ ]" }),
            };
        }
        if item.enabled { fg(ThemeColor::Success, "[x]") } else { fg(ThemeColor::Dim, "[ ]") }
    }

    fn item_suffix(&self, item: &ConfigItem) -> String {
        if !self.project_mode() {
            return String::new();
        }
        match self.editor.override_state(item) {
            ProjectOverrideState::Load => fg(ThemeColor::Muted, "  project load"),
            ProjectOverrideState::Unload => fg(ThemeColor::Muted, "  project unload"),
            ProjectOverrideState::Inherit if self.editor.is_inherited_global_item(item) => {
                fg(ThemeColor::Dim, "  inherited global")
            }
            ProjectOverrideState::Inherit => String::new(),
        }
    }

    fn is_dimmed(&self, item: &ConfigItem) -> bool {
        self.project_mode()
            && self.editor.is_inherited_global_item(item)
            && self.editor.override_state(item) == ProjectOverrideState::Inherit
    }

    fn render_entry(&self, entry: FlatEntry, selected: bool, width: usize) -> String {
        let groups = self.editor.groups();
        match entry {
            FlatEntry::Group(g) => {
                let group = &groups[g];
                let inherited = self.project_mode() && group.scope == SourceScope::User;
                let label = bold(&format!("{}{}", group.label, if inherited { " · inherited global" } else { "" }));
                let line = fg(if inherited { ThemeColor::Dim } else { ThemeColor::Accent }, &label);
                truncate(&format!("  {line}"), width, "")
            }
            FlatEntry::Subgroup(g, s) => {
                let inherited = self.project_mode() && groups[g].scope == SourceScope::User;
                let line =
                    fg(if inherited { ThemeColor::Dim } else { ThemeColor::Muted }, groups[g].subgroups[s].label);
                truncate(&format!("    {line}"), width, "")
            }
            FlatEntry::Item(g, s, i) => {
                let item = self.item(g, s, i);
                let cursor = if selected { "> " } else { "  " };
                let dimmed = self.is_dimmed(item);
                let name = if selected && !dimmed { bold(&item.display_name) } else { item.display_name.clone() };
                let name = if dimmed { fg(ThemeColor::Dim, &name) } else { name };
                let line = format!("{cursor}    {} {name}{}", self.render_checkbox(item), self.item_suffix(item));
                truncate(&line, width, "...")
            }
        }
    }
}

impl Component for ResourceList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.search_input.render(width);
        lines.push(String::new());
        if self.filtered_items.is_empty() {
            lines.push(fg(ThemeColor::Muted, "  No resources found"));
            return lines;
        }
        let len = self.filtered_items.len();
        let (start, end) = visible_window(self.selected_index, len, self.max_visible);
        for index in start..end {
            lines.push(self.render_entry(self.filtered_items[index], index == self.selected_index, width));
        }
        if start > 0 || end < len {
            let count = self.filtered_items.iter().filter(|entry| entry.is_item()).count();
            let current = self.filtered_items[..self.selected_index].iter().filter(|entry| entry.is_item()).count() + 1;
            lines.push(fg(ThemeColor::Dim, &format!("  ({current}/{count})")));
        }
        lines
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.search_input.focused = focused;
    }
}
