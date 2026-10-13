//! The autocomplete popup: a small select list that renders like upstream `SelectList`.

use super::SelectListTheme;
use crate::autocomplete::AutocompleteItem;
use crate::component::{TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType};
use crate::keybindings::{get_keybindings, ids};
use crate::utils::{truncate_to_width_with, visible_width};

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

/// Primary column bounds for slash command menus.
pub(super) const SLASH_COMMAND_COLUMN_BOUNDS: (usize, usize) = (12, 32);

fn single_line(text: &str) -> String {
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
    crate::utils::js_trim(&result).to_string()
}

fn truncate(text: &str, width: usize) -> String {
    truncate_to_width_with(text, width, "", false)
}

pub(crate) struct AutocompleteList {
    items: Vec<AutocompleteItem>,
    selected: usize,
    mouse_pressed: Option<usize>,
    max_visible: usize,
    theme: SelectListTheme,
    column_bounds: (usize, usize),
}

impl AutocompleteList {
    pub fn new(
        items: Vec<AutocompleteItem>,
        max_visible: usize,
        theme: SelectListTheme,
        column_bounds: Option<(usize, usize)>,
    ) -> Self {
        let bounds = column_bounds.unwrap_or((DEFAULT_PRIMARY_COLUMN_WIDTH, DEFAULT_PRIMARY_COLUMN_WIDTH));
        Self { items, selected: 0, mouse_pressed: None, max_visible, theme, column_bounds: bounds }
    }

    pub fn set_selected_index(&mut self, index: usize) {
        self.selected = index.min(self.items.len().saturating_sub(1));
    }

    pub fn selected_item(&self) -> Option<&AutocompleteItem> {
        self.items.get(self.selected)
    }

    fn visible_range(&self) -> (usize, usize) {
        let centered = self.selected.saturating_sub(self.max_visible / 2);
        let start = centered.min(self.items.len().saturating_sub(self.max_visible));
        (start, (start + self.max_visible).min(self.items.len()))
    }

    /// Up and Down move the selection, wrapping around.
    pub fn handle_key(&mut self, data: &str) {
        let kb = get_keybindings();
        let count = self.items.len();
        if count == 0 {
            return;
        }
        if kb.matches(data, ids::SELECT_UP) {
            self.selected = if self.selected == 0 { count - 1 } else { self.selected - 1 };
        } else if kb.matches(data, ids::SELECT_DOWN) {
            self.selected = if self.selected + 1 == count { 0 } else { self.selected + 1 };
        }
    }

    /// Wheel moves the selection; a click selects the item under the pointer.
    pub fn handle_mouse(&mut self, event: &TuiMouseEvent) -> (Option<TuiMouseEventResult>, Option<AutocompleteItem>) {
        if self.items.is_empty() {
            return (None, None);
        }
        if event.kind == TuiMouseEventType::Wheel
            && let Some(delta) = event.wheel_delta.filter(|delta| *delta != 0)
        {
            let previous = self.selected;
            self.selected =
                if delta < 0 { previous.saturating_sub(1) } else { (previous + 1).min(self.items.len() - 1) };
            let changed = self.selected != previous;
            let result = TuiMouseEventResult { handled: true, render: Some(changed), ..Default::default() };
            return (Some(result), None);
        }
        // Hover must not change the selection: the visible range is centered on it.
        let pressed_or_clicked = matches!(event.kind, TuiMouseEventType::Press | TuiMouseEventType::Click);
        if event.button != TuiMouseButton::Left || !pressed_or_clicked {
            return (None, None);
        }
        let (start, end) = self.visible_range();
        let index = start as i32 + event.y;
        if index < start as i32 || index >= end as i32 {
            return (None, None);
        }
        let index = index as usize;
        if event.kind == TuiMouseEventType::Press {
            self.mouse_pressed = Some(index);
            self.selected = index;
            return (Some(TuiMouseEventResult { handled: true, focus: true, ..Default::default() }), None);
        }
        self.selected = self.mouse_pressed.take().unwrap_or(index);
        (Some(TuiMouseEventResult::handled()), self.items.get(self.selected).cloned())
    }

    fn primary_column_width(&self) -> usize {
        let (min, max) = (
            self.column_bounds.0.min(self.column_bounds.1).max(1),
            self.column_bounds.0.max(self.column_bounds.1).max(1),
        );
        let widest =
            self.items.iter().map(|item| visible_width(display_value(item)) + PRIMARY_COLUMN_GAP).max().unwrap_or(0);
        widest.clamp(min, max)
    }

    pub fn render(&self, width: usize) -> Vec<String> {
        if self.items.is_empty() {
            return vec![(self.theme.no_match)("  No matching commands")];
        }
        let primary_width = self.primary_column_width();
        let (start, end) = self.visible_range();
        let mut lines: Vec<String> = (start..end)
            .map(|index| {
                let item = &self.items[index];
                let description = item.description.as_deref().map(single_line).filter(|d| !d.is_empty());
                self.render_item(item, index == self.selected, width, description.as_deref(), primary_width)
            })
            .collect();
        if start > 0 || end < self.items.len() {
            let scroll_text = format!("  ({}/{})", self.selected + 1, self.items.len());
            lines.push((self.theme.scroll_info)(&truncate(&scroll_text, width.saturating_sub(2))));
        }
        lines
    }

    fn render_item(
        &self,
        item: &AutocompleteItem,
        is_selected: bool,
        width: usize,
        description: Option<&str>,
        primary_width: usize,
    ) -> String {
        let prefix = if is_selected { "\u{2192} " } else { "  " };
        let prefix_width = visible_width(prefix);
        let value = display_value(item);

        if let Some(description) = description
            && width > 40
        {
            let column = primary_width.min(width.saturating_sub(prefix_width + 4)).max(1);
            let max_primary = column.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let truncated = truncate(value, max_primary);
            let truncated_width = visible_width(&truncated);
            let spacing = " ".repeat(column.saturating_sub(truncated_width).max(1));
            let description_start = prefix_width + truncated_width + spacing.len();
            let remaining = width as isize - description_start as isize - 2;
            if remaining > MIN_DESCRIPTION_WIDTH as isize {
                let description = truncate(description, remaining as usize);
                if is_selected {
                    return (self.theme.selected_text)(&format!("{prefix}{truncated}{spacing}{description}"));
                }
                return format!("{prefix}{truncated}{}", (self.theme.description)(&format!("{spacing}{description}")));
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2);
        let truncated = truncate(value, max_width);
        if is_selected {
            (self.theme.selected_text)(&format!("{prefix}{truncated}"))
        } else {
            format!("{prefix}{truncated}")
        }
    }
}

fn display_value(item: &AutocompleteItem) -> &str {
    if item.label.is_empty() { &item.value } else { &item.label }
}
