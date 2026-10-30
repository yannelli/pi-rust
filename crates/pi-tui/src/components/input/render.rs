//! Rendering: prompt, placeholder, horizontal scrolling, and the cursor.

use super::{Input, floor_char_boundary};
use crate::component::{CURSOR_MARKER, render_fake_cursor};
use crate::utils::{graphemes, slice_by_column, truncate_to_width_with, visible_width};

impl Input {
    /// Renders the single input line for `width` columns.
    pub(super) fn render_line(&mut self, width: usize) -> String {
        if !self.masked {
            return self.render_value(width);
        }
        let masked = "*".repeat(graphemes(&self.value).count());
        let masked_cursor = graphemes(&self.value[..self.cursor]).count();
        let value = std::mem::replace(&mut self.value, masked);
        let cursor = std::mem::replace(&mut self.cursor, masked_cursor);
        let line = self.render_value(width);
        self.value = value;
        self.cursor = cursor;
        line
    }

    fn render_value(&mut self, width: usize) -> String {
        let prompt_width = visible_width(&self.prompt);
        if width <= prompt_width {
            return truncate_to_width_with(&self.prompt, width, "", false);
        }
        let available_width = width - prompt_width;

        if self.value.is_empty() && !self.placeholder.is_empty() {
            return self.render_placeholder(available_width);
        }

        let (visible_text, cursor_display) = self.visible_window(available_width);
        let cursor_display = floor_char_boundary(&visible_text, cursor_display);
        let (before_cursor, rest) = visible_text.split_at(cursor_display);
        // The grapheme under the cursor, or a space when the cursor is at the end.
        let at_cursor = graphemes(rest).next().unwrap_or(" ");
        let after_cursor = rest.get(at_cursor.len()..).unwrap_or("");

        // The hardware cursor marker is zero width and precedes the fake cursor for IME placement.
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let text_with_cursor = format!("{before_cursor}{marker}{}{after_cursor}", render_fake_cursor(at_cursor));
        let padding = " ".repeat(available_width.saturating_sub(visible_width(&text_with_cursor)));
        format!("{}{text_with_cursor}{padding}", self.prompt)
    }

    fn render_placeholder(&self, available_width: usize) -> String {
        let placeholder = truncate_to_width_with(&self.placeholder, available_width, "", false);
        let at_cursor = graphemes(&placeholder).next().unwrap_or(" ");
        let after_cursor = placeholder.get(at_cursor.len()..).unwrap_or("");
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let style = &self.placeholder_style;
        let text_with_cursor = format!("{marker}{}{}", render_fake_cursor(&style(at_cursor)), style(after_cursor));
        let padding = " ".repeat(available_width.saturating_sub(visible_width(&text_with_cursor)));
        format!("{}{text_with_cursor}{padding}", self.prompt)
    }

    /// Returns the visible slice of the value and the cursor's byte offset in it, scrolling
    /// horizontally to keep the cursor in view. Records the first visible column for mouse hits.
    fn visible_window(&mut self, available_width: usize) -> (String, usize) {
        self.rendered_start_column = 0;
        let total_width = visible_width(&self.value);
        if total_width < available_width {
            // Everything fits, with room for the cursor at the end.
            return (self.value.clone(), self.cursor);
        }

        // Reserve one column for the cursor when it sits at the end.
        let scroll_width = if self.cursor == self.value.len() { available_width - 1 } else { available_width };
        if scroll_width == 0 {
            return (String::new(), 0);
        }
        let cursor_col = visible_width(&self.value[..self.cursor]);
        let half_width = scroll_width / 2;
        let start_col = if cursor_col < half_width {
            0
        } else if cursor_col > total_width.saturating_sub(half_width) {
            total_width.saturating_sub(scroll_width)
        } else {
            cursor_col.saturating_sub(half_width)
        };

        self.rendered_start_column = start_col;
        let visible_text = slice_by_column(&self.value, start_col, scroll_width, true);
        let before_cursor = slice_by_column(&self.value, start_col, cursor_col.saturating_sub(start_col), true);
        (visible_text, before_cursor.len())
    }
}
