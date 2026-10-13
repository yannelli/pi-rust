//! [`SelectList`]: a scrolling list of items with optional descriptions, keyboard and mouse selection.

use std::rc::Rc;

use crate::component::{
    Component, MouseResponse, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
};
use crate::components::scroll_view::StyleFn;
use crate::keybindings::{get_keybindings, ids};
use crate::utils::{js_trim, truncate_to_width_with, visible_width};

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: i64 = 10;

/// One entry of a [`SelectList`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl SelectItem {
    pub fn new(value: &str, label: &str) -> Self {
        Self { value: value.to_string(), label: label.to_string(), description: None }
    }

    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }
}

/// Styling of a [`SelectList`].
#[derive(Clone)]
pub struct SelectListTheme {
    pub selected_prefix: StyleFn,
    pub selected_text: StyleFn,
    pub description: StyleFn,
    pub scroll_info: StyleFn,
    pub no_match: StyleFn,
}

/// What a custom primary-column truncation receives.
pub struct SelectListTruncatePrimaryContext<'a> {
    pub text: &'a str,
    pub max_width: usize,
    pub column_width: usize,
    pub item: &'a SelectItem,
    pub is_selected: bool,
}

/// Custom truncation of the primary column.
pub type TruncatePrimaryFn = Rc<dyn Fn(&SelectListTruncatePrimaryContext<'_>) -> String>;

/// Column layout of a [`SelectList`].
#[derive(Clone, Default)]
pub struct SelectListLayoutOptions {
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
    pub truncate_primary: Option<TruncatePrimaryFn>,
}

/// Callback receiving a selected item.
pub type SelectItemCallback = Box<dyn FnMut(&SelectItem)>;

/// A list with a highlighted selection (upstream `SelectList`). Callbacks run while the list is
/// borrowed, so they must not borrow the list again.
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_items: Vec<SelectItem>,
    selected_index: usize,
    mouse_pressed_index: Option<usize>,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayoutOptions,
    pub on_select: Option<SelectItemCallback>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    pub on_selection_change: Option<SelectItemCallback>,
}

fn normalize_to_single_line(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_break = false;
    for c in text.chars() {
        if c == '\r' || c == '\n' {
            if !in_break {
                result.push(' ');
            }
            in_break = true;
        } else {
            result.push(c);
            in_break = false;
        }
    }
    js_trim(&result).to_string()
}

impl SelectList {
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayoutOptions,
    ) -> Self {
        Self {
            filtered_items: items.clone(),
            items,
            selected_index: 0,
            mouse_pressed_index: None,
            max_visible,
            theme,
            layout,
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
        }
    }

    /// Keeps items whose value starts with `filter` (case-insensitive) and resets the selection.
    pub fn set_filter(&mut self, filter: &str) {
        let filter = filter.to_lowercase();
        self.filtered_items =
            self.items.iter().filter(|item| item.value.to_lowercase().starts_with(&filter)).cloned().collect();
        self.selected_index = 0;
    }

    pub fn set_selected_index(&mut self, index: usize) {
        self.selected_index = index.min(self.filtered_items.len().saturating_sub(1));
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn get_selected_item(&self) -> Option<&SelectItem> {
        self.filtered_items.get(self.selected_index)
    }

    pub fn filtered_items(&self) -> &[SelectItem] {
        &self.filtered_items
    }

    fn visible_range(&self) -> (usize, usize) {
        let len = self.filtered_items.len() as i64;
        let max_visible = self.max_visible as i64;
        let start = (self.selected_index as i64 - max_visible / 2).min(len - max_visible).max(0);
        let end = (start + max_visible).min(len);
        (start as usize, end.max(start) as usize)
    }

    fn notify_selection_change(&mut self) {
        if let Some(item) = self.filtered_items.get(self.selected_index)
            && let Some(callback) = self.on_selection_change.as_mut()
        {
            callback(item);
        }
    }

    fn notify_select(&mut self) {
        if let Some(item) = self.filtered_items.get(self.selected_index)
            && let Some(callback) = self.on_select.as_mut()
        {
            callback(item);
        }
    }

    fn display_value(item: &SelectItem) -> &str {
        if item.label.is_empty() { &item.value } else { &item.label }
    }

    fn primary_column_bounds(&self) -> (usize, usize) {
        let min = self.layout.min_primary_column_width;
        let max = self.layout.max_primary_column_width;
        let raw_min = min.or(max).unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = max.or(min).unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        (raw_min.min(raw_max).max(1), raw_min.max(raw_max).max(1))
    }

    fn primary_column_width(&self) -> usize {
        let (min, max) = self.primary_column_bounds();
        let widest = self
            .filtered_items
            .iter()
            .map(|item| visible_width(Self::display_value(item)) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        widest.clamp(min, max)
    }

    fn truncate_primary(&self, item: &SelectItem, is_selected: bool, max_width: usize, column_width: usize) -> String {
        let text = Self::display_value(item);
        let truncated = match &self.layout.truncate_primary {
            Some(truncate) => {
                truncate(&SelectListTruncatePrimaryContext { text, max_width, column_width, item, is_selected })
            }
            None => truncate_to_width_with(text, max_width, "", false),
        };
        truncate_to_width_with(&truncated, max_width, "", false)
    }

    fn render_item(&self, item: &SelectItem, is_selected: bool, width: usize, primary_column_width: usize) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);
        let description = item.description.as_deref().map(normalize_to_single_line).unwrap_or_default();

        if !description.is_empty() && width > 40 {
            let effective_column = primary_column_width.min(width - prefix_width - 4).max(1);
            let max_primary_width = effective_column.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let value = self.truncate_primary(item, is_selected, max_primary_width, effective_column);
            let value_width = visible_width(&value);
            let spacing = " ".repeat(effective_column.saturating_sub(value_width).max(1));
            let description_start = prefix_width + value_width + spacing.len();
            let remaining_width = width as i64 - description_start as i64 - 2;
            if remaining_width > MIN_DESCRIPTION_WIDTH {
                let description = truncate_to_width_with(&description, remaining_width as usize, "", false);
                if is_selected {
                    return (self.theme.selected_text)(&format!("{prefix}{value}{spacing}{description}"));
                }
                return format!("{prefix}{value}{}", (self.theme.description)(&format!("{spacing}{description}")));
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2);
        let value = self.truncate_primary(item, is_selected, max_width, max_width);
        if is_selected { (self.theme.selected_text)(&format!("{prefix}{value}")) } else { format!("{prefix}{value}") }
    }
}

impl Component for SelectList {
    fn render(&mut self, width: usize) -> Vec<String> {
        if self.filtered_items.is_empty() {
            return vec![(self.theme.no_match)("  No matching commands")];
        }
        let primary_column_width = self.primary_column_width();
        let (start, end) = self.visible_range();
        let mut lines: Vec<String> = (start..end)
            .map(|index| {
                self.render_item(&self.filtered_items[index], index == self.selected_index, width, primary_column_width)
            })
            .collect();
        if start > 0 || end < self.filtered_items.len() {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, self.filtered_items.len());
            lines.push((self.theme.scroll_info)(&truncate_to_width_with(
                &scroll_text,
                width.saturating_sub(2),
                "",
                false,
            )));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        let len = self.filtered_items.len();
        if kb.matches(data, ids::SELECT_UP) {
            if len == 0 {
                return;
            }
            self.selected_index = if self.selected_index == 0 { len - 1 } else { self.selected_index - 1 };
            self.notify_selection_change();
        } else if kb.matches(data, ids::SELECT_DOWN) {
            if len == 0 {
                return;
            }
            self.selected_index = if self.selected_index + 1 >= len { 0 } else { self.selected_index + 1 };
            self.notify_selection_change();
        } else if kb.matches(data, ids::SELECT_CONFIRM) {
            self.notify_select();
        } else if kb.matches(data, ids::SELECT_CANCEL)
            && let Some(on_cancel) = self.on_cancel.as_mut()
        {
            on_cancel();
        }
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let len = self.filtered_items.len();
        if len == 0 {
            return None;
        }
        if event.kind == TuiMouseEventType::Wheel
            && let Some(delta) = event.wheel_delta.filter(|delta| *delta != 0)
        {
            let previous = self.selected_index;
            self.selected_index =
                if delta < 0 { self.selected_index.saturating_sub(1) } else { (self.selected_index + 1).min(len - 1) };
            let changed = self.selected_index != previous;
            if changed {
                self.notify_selection_change();
            }
            return Some(MouseResponse::Handled(TuiMouseEventResult {
                handled: true,
                render: Some(changed),
                ..TuiMouseEventResult::default()
            }));
        }
        // Hover must not change selection: the visible range is centered on it.
        let is_press = event.kind == TuiMouseEventType::Press;
        if event.button != TuiMouseButton::Left || !(is_press || event.kind == TuiMouseEventType::Click) {
            return None;
        }
        let (start, end) = self.visible_range();
        let index = start as i64 + i64::from(event.y);
        if index < start as i64 || index >= end as i64 {
            return None;
        }
        let index = index as usize;
        if is_press {
            self.mouse_pressed_index = Some(index);
            if self.selected_index != index {
                self.selected_index = index;
                self.notify_selection_change();
            }
            return Some(MouseResponse::Handled(TuiMouseEventResult {
                handled: true,
                focus: true,
                ..TuiMouseEventResult::default()
            }));
        }
        let clicked = self.mouse_pressed_index.take().unwrap_or(index);
        let changed = self.selected_index != clicked;
        self.selected_index = clicked;
        if changed {
            self.notify_selection_change();
        }
        self.notify_select();
        Some(MouseResponse::Handled(TuiMouseEventResult::handled()))
    }
}
