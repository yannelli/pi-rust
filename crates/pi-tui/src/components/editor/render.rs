//! Layout, rendering, scroll borders, and mouse handling.

use super::state::EditorCore;
use super::wrap::word_wrap_line_with_segments;
use crate::component::{
    CURSOR_MARKER, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType, render_fake_cursor,
};
use crate::components::scroll_view::StyleFn;
use crate::utils::{slice_by_column, visible_width};

/// Inputs for a custom border.
pub struct BorderContext<'a> {
    pub width: usize,
    /// Lines hidden above (top border) or below (bottom border).
    pub hidden_line_count: usize,
    pub border_color: &'a StyleFn,
    pub is_top: bool,
}

impl BorderContext<'_> {
    /// The border the editor draws by default.
    pub fn default_border(&self) -> String {
        let direction = if self.is_top { "\u{2191}" } else { "\u{2193}" };
        let border = if self.hidden_line_count > 0 {
            scroll_border(direction, self.hidden_line_count, self.width)
        } else {
            "\u{2500}".repeat(self.width)
        };
        (self.border_color)(&border)
    }
}

/// Draws a border line in place of the default one. It runs during render, so it must not call
/// back into the editor.
pub type BorderRenderer = Box<dyn FnMut(&BorderContext<'_>) -> String>;

/// A border with a centered `↑ N more` label, shortened to fit narrow widths.
pub fn scroll_border(direction: &str, hidden_line_count: usize, width: usize) -> String {
    let label = format!(" {direction} {hidden_line_count} more ");
    let label_width = visible_width(&label);
    if label_width + 2 <= width {
        let left = (width - label_width) / 2;
        return format!("{}{label}{}", "\u{2500}".repeat(left), "\u{2500}".repeat(width - left - label_width));
    }
    let indicator = format!("\u{2500}\u{2500}\u{2500} {direction} {hidden_line_count} more ");
    let indicator_width = visible_width(&indicator);
    if indicator_width <= width {
        return format!("{indicator}{}", "\u{2500}".repeat(width - indicator_width));
    }
    let ellipsis = &"..."[..width.min(3)];
    format!("{}{ellipsis}", slice_by_column(&indicator, 0, width - ellipsis.len(), true))
}

/// A visual line ready to draw, with the cursor byte offset when it holds the cursor.
struct LayoutLine {
    text: String,
    cursor: Option<usize>,
}

/// Horizontal padding that fits `width`, and the content width inside it.
fn padding_and_content(padding_x: usize, width: usize) -> (usize, usize) {
    let padding = padding_x.min(width.saturating_sub(1) / 2);
    (padding, width.saturating_sub(padding * 2).max(1))
}

impl EditorCore {
    /// Visible text rows: 30% of the terminal height, at least 5.
    pub fn max_visible_lines(&self) -> usize {
        ((self.terminal_rows() as f64 * 0.3).floor() as usize).max(5)
    }

    fn layout_text(&self, width: usize) -> Vec<LayoutLine> {
        if self.is_editor_empty() {
            return vec![LayoutLine { text: String::new(), cursor: Some(0) }];
        }
        let mut layout = Vec::new();
        for (index, line) in self.state.lines.iter().enumerate() {
            let is_current = index == self.state.cursor_line;
            if visible_width(line) <= width {
                layout.push(LayoutLine { text: line.clone(), cursor: is_current.then_some(self.state.cursor_col) });
                continue;
            }
            let segments = self.graphemes(line);
            let chunks = word_wrap_line_with_segments(line, width, Some(&segments));
            let last = chunks.len() - 1;
            for (chunk_index, chunk) in chunks.into_iter().enumerate() {
                let col = self.state.cursor_col;
                // A non-last chunk holds the cursor only before its end; the last takes the rest.
                let has_cursor =
                    is_current && col >= chunk.start_index && (chunk_index == last || col < chunk.end_index);
                let cursor = has_cursor.then(|| (col - chunk.start_index).min(chunk.text.len()));
                layout.push(LayoutLine { text: chunk.text, cursor });
            }
        }
        layout
    }

    fn render_border(&mut self, width: usize, hidden_line_count: usize, is_top: bool) -> String {
        let border_color = self.border_color.clone();
        let context = BorderContext { width, hidden_line_count, border_color: &border_color, is_top };
        let renderer = if is_top { &mut self.top_border } else { &mut self.bottom_border };
        match renderer {
            Some(renderer) => renderer(&context),
            None => context.default_border(),
        }
    }

    pub fn render(&mut self, width: usize) -> Vec<String> {
        let (padding_x, content_width) = padding_and_content(self.padding_x, width);
        // Without padding one column is kept free for the cursor; with padding it may overflow.
        let layout_width = content_width.saturating_sub(if padding_x > 0 { 0 } else { 1 }).max(1);
        self.last_width = layout_width;

        let layout = self.layout_text(layout_width);
        let max_visible = self.max_visible_lines();
        let cursor_index = layout.iter().position(|line| line.cursor.is_some()).unwrap_or(0);
        if cursor_index < self.scroll_offset {
            self.scroll_offset = cursor_index;
        } else if cursor_index >= self.scroll_offset + max_visible {
            self.scroll_offset = cursor_index + 1 - max_visible;
        }
        self.scroll_offset = self.scroll_offset.min(layout.len().saturating_sub(max_visible));

        let end = (self.scroll_offset + max_visible).min(layout.len());
        let visible = &layout[self.scroll_offset..end];
        self.rendered_visible_line_count = visible.len();

        let side_padding = " ".repeat(padding_x);
        let mut result = vec![self.render_border(width, self.scroll_offset, true)];
        // The hardware cursor marker helps IME placement even while the popup is open.
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        for line in visible {
            let mut display = line.text.clone();
            let mut line_width = visible_width(&line.text);
            let mut cursor_in_padding = false;
            if let Some(cursor) = line.cursor {
                let mut cursor = cursor.min(line.text.len());
                while !line.text.is_char_boundary(cursor) {
                    cursor -= 1;
                }
                let (before, after) = line.text.split_at(cursor);
                if let Some(first) = self.graphemes(after).first() {
                    let rest = &after[first.segment.len()..];
                    display = format!("{before}{marker}{}{rest}", render_fake_cursor(first.segment));
                } else {
                    display = format!("{before}{marker}{}", render_fake_cursor(" "));
                    line_width += 1;
                    cursor_in_padding = line_width > content_width && padding_x > 0;
                }
            }
            let fill = " ".repeat(content_width.saturating_sub(line_width));
            let right = if cursor_in_padding { &side_padding[1..] } else { &side_padding[..] };
            result.push(format!("{side_padding}{display}{fill}{right}"));
        }
        let below = layout.len() - end;
        result.push(self.render_border(width, below, false));
        if let Some(line) = self.render_history_search(content_width) {
            result.push(format!("{side_padding}{line}{side_padding}"));
        }

        self.rendered_autocomplete_height = 0;
        if self.autocomplete_mode.is_some()
            && let Some(list) = &self.autocomplete_list
        {
            let lines = list.render(content_width);
            self.rendered_autocomplete_height = lines.len();
            for line in lines {
                let fill = " ".repeat(content_width.saturating_sub(visible_width(&line)));
                result.push(format!("{side_padding}{line}{fill}{side_padding}"));
            }
        }
        result
    }

    pub fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        let popup_start = self.rendered_visible_line_count as i32 + 2;
        if self.autocomplete_mode.is_some()
            && self.autocomplete_list.is_some()
            && event.y >= popup_start
            && event.y < popup_start + self.rendered_autocomplete_height as i32
        {
            let (padding_x, content_width) = padding_and_content(self.padding_x, event.width.max(0) as usize);
            let local = TuiMouseEvent {
                x: event.x - padding_x as i32,
                y: event.y - popup_start,
                width: content_width as i32,
                height: self.rendered_autocomplete_height as i32,
                ..event.clone()
            };
            let list = self.autocomplete_list.as_mut()?;
            let (result, selected) = list.handle_mouse(&local);
            if let Some(item) = selected {
                self.accept_completion(&item);
            }
            return result.map(|result| TuiMouseEventResult { focus: true, ..result });
        }

        // Press, drag, and release stay unhandled so the renderer can select text; the click
        // it synthesizes for a press and release in place positions the cursor.
        if event.kind != TuiMouseEventType::Click || event.button != TuiMouseButton::Left {
            return None;
        }
        let focus = TuiMouseEventResult { handled: true, focus: true, ..TuiMouseEventResult::default() };
        if event.y <= 0 || event.y > self.rendered_visible_line_count as i32 {
            return Some(focus);
        }
        let visual_lines = self.build_visual_line_map(self.last_width);
        let index = self.scroll_offset + event.y as usize - 1;
        let Some(&visual) = visual_lines.get(index) else { return Some(focus) };
        let logical = self.state.lines.get(visual.logical_line).cloned().unwrap_or_default();
        let chunk = &logical[visual.start_col..visual.start_col + visual.length];
        let (padding_x, _) = padding_and_content(self.padding_x, event.width.max(0) as usize);
        let target_column = (event.x - padding_x as i32).max(0) as usize;

        let mut column = 0;
        let mut target_index = chunk.len();
        let mut last_index = 0;
        for segment in self.graphemes(chunk) {
            let next_column = column + visible_width(segment.segment);
            last_index = segment.index;
            if target_column < next_column {
                target_index = segment.index;
                break;
            }
            column = next_column;
        }
        let is_last = visual_lines.get(index + 1).is_none_or(|next| next.logical_line != visual.logical_line);
        // A click past the end of a wrapped (non-last) row stays on that row.
        if !is_last && target_index == chunk.len() && !chunk.is_empty() {
            target_index = last_index;
        }

        self.state.cursor_line = visual.logical_line;
        self.set_cursor_col(visual.start_col + target_index);
        self.last_action = None;
        self.exit_history_browsing();
        if self.autocomplete_mode.is_some() {
            self.update_autocomplete();
        }
        Some(focus)
    }
}
