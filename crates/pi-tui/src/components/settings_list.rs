//! [`SettingsList`]: a list of settings with cycling values, submenus, search, and mouse support.

use std::cell::RefCell;
use std::rc::Rc;

use crate::component::{
    Component, ComponentRef, MouseResponse, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
};
use crate::components::input::{Input, InputOptions};
use crate::components::scroll_view::StyleFn;
use crate::fuzzy::fuzzy_filter;
use crate::keybindings::{get_keybindings, ids};
use crate::utils::{truncate_to_width, truncate_to_width_with, visible_width, wrap_text_with_ansi};

/// Builds a submenu for an item from its current value and a [`SubmenuDone`] handle.
pub type SubmenuFactory = Rc<dyn Fn(&str, SubmenuDone) -> ComponentRef>;

/// One setting of a [`SettingsList`].
#[derive(Clone, Default)]
pub struct SettingItem {
    /// Unique identifier.
    pub id: String,
    /// Display label (left side).
    pub label: String,
    /// Shown when the item is selected.
    pub description: Option<String>,
    /// Current value (right side).
    pub current_value: String,
    /// If set, Enter or Space cycles through these values.
    pub values: Option<Vec<String>>,
    /// If set, Enter opens this submenu.
    pub submenu: Option<SubmenuFactory>,
}

/// Styling of a [`SettingsList`]. `label` and `value` also receive whether the row is selected.
pub type SelectedStyleFn = Rc<dyn Fn(&str, bool) -> String>;

#[derive(Clone)]
pub struct SettingsListTheme {
    pub label: SelectedStyleFn,
    pub value: SelectedStyleFn,
    pub description: StyleFn,
    pub cursor: String,
    pub hint: StyleFn,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettingsListOptions {
    pub enable_search: bool,
}

/// Options of [`SubmenuDone::done_with`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubmenuDoneOptions {
    /// Moves the cursor to the item with this id after closing, and opens its submenu.
    pub navigate_to: Option<String>,
}

struct SubmenuClose {
    item: usize,
    selected_value: Option<String>,
    options: SubmenuDoneOptions,
}

/// Closes the submenu it was created for. Safe to call from inside the submenu's own handlers:
/// the list applies the close as soon as control returns to it (input, mouse, or render).
#[derive(Clone)]
pub struct SubmenuDone {
    item: usize,
    queue: Rc<RefCell<Vec<SubmenuClose>>>,
}

impl SubmenuDone {
    /// Closes the submenu, storing `selected_value` (if any) as the item's new value.
    pub fn done(&self, selected_value: Option<&str>) {
        self.done_with(selected_value, SubmenuDoneOptions::default());
    }

    pub fn done_with(&self, selected_value: Option<&str>, options: SubmenuDoneOptions) {
        let selected_value = selected_value.map(str::to_string);
        self.queue.borrow_mut().push(SubmenuClose { item: self.item, selected_value, options });
    }
}

/// Called with an item id and its new value.
pub type SettingChangeCallback = Box<dyn FnMut(&str, &str)>;

/// Settings list (upstream `SettingsList`).
pub struct SettingsList {
    items: Vec<SettingItem>,
    /// Indices into `items` matching the search query.
    filtered: Vec<usize>,
    theme: SettingsListTheme,
    selected_index: usize,
    mouse_pressed_index: Option<usize>,
    max_visible: usize,
    on_change: SettingChangeCallback,
    on_cancel: Box<dyn FnMut()>,
    search_input: Option<Input>,
    submenu: Option<ComponentRef>,
    submenu_item_index: Option<usize>,
    navigate_after_close: Option<String>,
    closes: Rc<RefCell<Vec<SubmenuClose>>>,
}

impl SettingsList {
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        on_change: impl FnMut(&str, &str) + 'static,
        on_cancel: impl FnMut() + 'static,
        options: SettingsListOptions,
    ) -> Self {
        Self {
            filtered: (0..items.len()).collect(),
            items,
            theme,
            selected_index: 0,
            mouse_pressed_index: None,
            max_visible,
            on_change: Box::new(on_change),
            on_cancel: Box::new(on_cancel),
            search_input: options.enable_search.then(|| Input::new(InputOptions::default())),
            submenu: None,
            submenu_item_index: None,
            navigate_after_close: None,
            closes: Rc::default(),
        }
    }

    pub fn items(&self) -> &[SettingItem] {
        &self.items
    }

    /// Updates an item's current value.
    pub fn update_value(&mut self, id: &str, new_value: &str) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.current_value = new_value.to_string();
        }
    }

    /// Moves the selection to the item with the given id (no-op if not found).
    pub fn select_item(&mut self, id: &str) {
        if let Some(index) = self.display_items().iter().position(|&i| self.items[i].id == id) {
            self.selected_index = index;
        }
    }

    fn search_enabled(&self) -> bool {
        self.search_input.is_some()
    }

    fn display_items(&self) -> Vec<usize> {
        if self.search_enabled() { self.filtered.clone() } else { (0..self.items.len()).collect() }
    }

    fn visible_range(&self, len: usize) -> (usize, usize) {
        let max_visible = self.max_visible as i64;
        let start = (self.selected_index as i64 - max_visible / 2).min(len as i64 - max_visible).max(0);
        (start as usize, (start + max_visible).min(len as i64).max(start) as usize)
    }

    /// Applies submenu closes requested through [`SubmenuDone`].
    fn apply_submenu_closes(&mut self) {
        loop {
            let close = {
                let mut queue = self.closes.borrow_mut();
                if queue.is_empty() {
                    return;
                }
                queue.remove(0)
            };
            if let Some(value) = close.selected_value
                && let Some(item) = self.items.get_mut(close.item)
            {
                item.current_value = value.clone();
                (self.on_change)(&item.id, &value);
            }
            if let Some(target) = close.options.navigate_to.filter(|id| !id.is_empty()) {
                self.navigate_after_close = Some(target);
            }
            self.close_submenu();
        }
    }

    fn activate_item(&mut self) {
        let Some(&item_index) = self.display_items().get(self.selected_index) else {
            return;
        };
        let item = &mut self.items[item_index];
        if let Some(factory) = item.submenu.clone() {
            self.submenu_item_index = Some(self.selected_index);
            let done = SubmenuDone { item: item_index, queue: self.closes.clone() };
            let current_value = item.current_value.clone();
            self.submenu = Some(factory(&current_value, done));
        } else if let Some(values) = item.values.as_ref().filter(|values| !values.is_empty()) {
            let next =
                values.iter().position(|value| *value == item.current_value).map_or(0, |i| (i + 1) % values.len());
            item.current_value = values[next].clone();
            (self.on_change)(&item.id, &item.current_value);
        }
    }

    fn close_submenu(&mut self) {
        self.submenu = None;
        if let Some(id) = self.navigate_after_close.take() {
            self.submenu_item_index = None;
            self.select_item(&id);
            // Open the target item's submenu automatically.
            self.activate_item();
        } else if let Some(index) = self.submenu_item_index.take() {
            self.selected_index = index;
        }
    }

    fn apply_filter(&mut self, query: &str) {
        let labels: Vec<(usize, &str)> =
            self.items.iter().enumerate().map(|(i, item)| (i, item.label.as_str())).collect();
        self.filtered = fuzzy_filter(labels, query, |(_, label)| label).into_iter().map(|(i, _)| i).collect();
        self.selected_index = 0;
    }

    fn add_hint_line(&self, lines: &mut Vec<String>, width: usize) {
        let hint = if self.search_enabled() {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        lines.push(String::new());
        lines.push(truncate_to_width(&(self.theme.hint)(hint), width));
    }

    fn render_main_list(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(input) = self.search_input.as_mut() {
            lines.extend(input.render(width));
            lines.push(String::new());
        }
        if self.items.is_empty() {
            lines.push((self.theme.hint)("  No settings available"));
            if self.search_enabled() {
                self.add_hint_line(&mut lines, width);
            }
            return lines;
        }
        let display = self.display_items();
        if display.is_empty() {
            lines.push(truncate_to_width(&(self.theme.hint)("  No matching settings"), width));
            self.add_hint_line(&mut lines, width);
            return lines;
        }

        let (start, end) = self.visible_range(display.len());
        let max_label_width = self.items.iter().map(|item| visible_width(&item.label)).max().unwrap_or(0).min(36);
        for (index, &item_index) in display.iter().enumerate().take(end).skip(start) {
            let item = &self.items[item_index];
            let is_selected = index == self.selected_index;
            let prefix = if is_selected { self.theme.cursor.as_str() } else { "  " };
            let label_padded =
                format!("{}{}", item.label, " ".repeat(max_label_width.saturating_sub(visible_width(&item.label))));
            let label = (self.theme.label)(&label_padded, is_selected);
            let separator = "  ";
            let used_width = visible_width(prefix) + max_label_width + visible_width(separator);
            let value_max_width = width.saturating_sub(used_width + 2);
            let value = (self.theme.value)(
                &truncate_to_width_with(&item.current_value, value_max_width, "", false),
                is_selected,
            );
            lines.push(truncate_to_width(&format!("{prefix}{label}{separator}{value}"), width));
        }

        if start > 0 || end < display.len() {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, display.len());
            lines.push((self.theme.hint)(&truncate_to_width_with(&scroll_text, width.saturating_sub(2), "", false)));
        }

        if let Some(description) = display.get(self.selected_index).and_then(|&i| self.items[i].description.as_deref())
        {
            lines.push(String::new());
            for line in wrap_text_with_ansi(description, width.saturating_sub(4)) {
                lines.push((self.theme.description)(&format!("  {line}")));
            }
        }

        self.add_hint_line(&mut lines, width);
        lines
    }
}

fn with_focus(response: MouseResponse) -> MouseResponse {
    match response {
        MouseResponse::Handled(result) => MouseResponse::Handled(TuiMouseEventResult { focus: true, ..result }),
        MouseResponse::Forwarded(mut forwarded) => {
            forwarded.result.focus = true;
            MouseResponse::Forwarded(forwarded)
        }
    }
}

impl Component for SettingsList {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.apply_submenu_closes();
        if let Some(submenu) = &self.submenu {
            return submenu.borrow_mut().render(width);
        }
        self.render_main_list(width)
    }

    fn handle_input(&mut self, data: &str) {
        self.apply_submenu_closes();
        // While a submenu is open it receives all input; its cancel handler closes it through done().
        if let Some(submenu) = self.submenu.clone() {
            submenu.borrow_mut().handle_input(data);
            self.apply_submenu_closes();
            return;
        }

        let kb = get_keybindings();
        let len = self.display_items().len();
        let search_empty = self.search_input.as_ref().is_none_or(|input| input.get_value().is_empty());
        if kb.matches(data, ids::SELECT_UP) {
            if len > 0 {
                self.selected_index = if self.selected_index == 0 { len - 1 } else { self.selected_index - 1 };
            }
        } else if kb.matches(data, ids::SELECT_DOWN) {
            if len > 0 {
                self.selected_index = if self.selected_index + 1 >= len { 0 } else { self.selected_index + 1 };
            }
        } else if kb.matches(data, ids::SELECT_CONFIRM) || (data == " " && search_empty) {
            self.activate_item();
        } else if kb.matches(data, ids::SELECT_CANCEL) {
            (self.on_cancel)();
        } else if let Some(input) = self.search_input.as_mut() {
            input.handle_input(data);
            let query = input.get_value().to_string();
            self.apply_filter(&query);
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        self.apply_submenu_closes();
        if let Some(submenu) = self.submenu.clone() {
            let response = submenu.borrow_mut().handle_mouse(event).map(with_focus);
            self.apply_submenu_closes();
            return response;
        }
        if let Some(input) = self.search_input.as_mut() {
            if event.y == 0 {
                return input.handle_mouse(event).map(with_focus);
            }
            if event.y == 1 {
                return None;
            }
        }

        let len = self.display_items().len();
        if len == 0 {
            return None;
        }
        if event.kind == TuiMouseEventType::Wheel
            && let Some(delta) = event.wheel_delta.filter(|delta| *delta != 0)
        {
            let previous = self.selected_index;
            self.selected_index =
                if delta < 0 { self.selected_index.saturating_sub(1) } else { (self.selected_index + 1).min(len - 1) };
            return Some(MouseResponse::Handled(TuiMouseEventResult {
                handled: true,
                render: Some(self.selected_index != previous),
                ..TuiMouseEventResult::default()
            }));
        }
        // Hover must not change selection: the visible range is centered on it.
        let is_press = event.kind == TuiMouseEventType::Press;
        if event.button != TuiMouseButton::Left || !(is_press || event.kind == TuiMouseEventType::Click) {
            return None;
        }
        let row_offset = if self.search_enabled() { 2 } else { 0 };
        let (start, end) = self.visible_range(len);
        let index = start as i64 + i64::from(event.y) - row_offset;
        if index < start as i64 || index >= end as i64 {
            return None;
        }
        let index = index as usize;
        if is_press {
            self.mouse_pressed_index = Some(index);
            self.selected_index = index;
            return Some(MouseResponse::Handled(TuiMouseEventResult {
                handled: true,
                focus: true,
                ..TuiMouseEventResult::default()
            }));
        }
        self.selected_index = self.mouse_pressed_index.take().unwrap_or(index);
        self.activate_item();
        Some(MouseResponse::Handled(TuiMouseEventResult::handled()))
    }

    fn invalidate(&mut self) {
        if let Some(submenu) = &self.submenu {
            submenu.borrow_mut().invalidate();
        }
    }
}
