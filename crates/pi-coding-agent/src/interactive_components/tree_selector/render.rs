//! Rendering of [`TreeList`] rows: tree prefix with gutters and fold markers, active path
//! marker, labels, and a horizontal viewport that keeps deep rows readable.

use pi_tui::Component;
use pi_tui::utils::{slice_by_column, truncate_to_width, truncate_to_width_with, visible_width};

use super::list::{FilterMode, TreeList};
use super::text::{entry_display_text, format_label_timestamp};
use crate::interactive_components::common::visible_window;
use crate::theme::{ThemeBg, ThemeColor, theme};

const TREE_GUTTER_WIDTH: usize = 2;
const MIN_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 4;
const MAX_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 20;
const MIN_ANCHOR_CONTEXT_WIDTH: usize = 2;
const MAX_ANCHOR_CONTEXT_WIDTH: usize = 12;

struct ViewportRow {
    gutter: String,
    body: String,
    anchor_col: usize,
    body_width: usize,
    is_selected: bool,
}

/// Clips row bodies horizontally, keeping the cursor gutter. Bodies shift left only when the
/// selected row's text would start too far right to show useful content.
fn render_horizontal_viewport(rows: &[ViewportRow], width: usize) -> Vec<String> {
    let viewport_width = width.saturating_sub(TREE_GUTTER_WIDTH);
    let max_body_width = rows.iter().map(|row| row.body_width).max().unwrap_or(0);
    let max_scroll = max_body_width.saturating_sub(viewport_width);
    let mut scroll = 0;
    if let Some(selected) = rows.iter().find(|row| row.is_selected)
        && max_scroll > 0
    {
        let min_content =
            (viewport_width / 3).clamp(MIN_VISIBLE_ANCHOR_CONTENT_WIDTH, MAX_VISIBLE_ANCHOR_CONTENT_WIDTH);
        if selected.anchor_col as i64 > viewport_width as i64 - min_content as i64 {
            let context = (viewport_width / 4).clamp(MIN_ANCHOR_CONTEXT_WIDTH, MAX_ANCHOR_CONTEXT_WIDTH);
            scroll = max_scroll.min(selected.anchor_col.saturating_sub(context));
        }
    }
    rows.iter()
        .map(|row| {
            let line = if scroll > 0 {
                format!("{}{}\x1b[0m", row.gutter, slice_by_column(&row.body, scroll, viewport_width, true))
            } else {
                format!("{}{}", row.gutter, row.body)
            };
            truncate_to_width_with(&line, width, "", false)
        })
        .collect()
}

impl TreeList {
    fn status_labels(&self) -> String {
        let mut labels = match self.filter_mode {
            FilterMode::NoTools => " [no-tools]",
            FilterMode::UserOnly => " [user]",
            FilterMode::LabeledOnly => " [labeled]",
            FilterMode::All => " [all]",
            FilterMode::Default => "",
        }
        .to_string();
        if self.show_label_timestamps {
            labels.push_str(" [+label time]");
        }
        labels
    }

    /// The tree prefix of a row: gutters of ancestor branches and its own connector, whose
    /// middle shows `⊞` when folded and `⊟` when foldable.
    fn row_prefix(&self, flat: usize) -> String {
        let row = &self.flat_nodes[flat];
        let display_indent = if self.multiple_roots { row.indent.saturating_sub(1) } else { row.indent };
        let has_connector = row.show_connector && !row.is_virtual_root_child;
        let connector_level = if has_connector { display_indent.checked_sub(1) } else { None };
        let folded = self.folded.contains(self.node_of(flat).entry.id());
        let mut prefix = String::new();
        for i in 0..display_indent * 3 {
            let (level, column) = (i / 3, i % 3);
            let ch = if let Some(gutter) = row.gutters.iter().find(|gutter| gutter.position == level) {
                if column == 0 && gutter.show { '│' } else { ' ' }
            } else if connector_level == Some(level) {
                match column {
                    0 if row.is_last => '└',
                    0 => '├',
                    1 if folded => '⊞',
                    1 if self.is_foldable(flat) => '⊟',
                    1 => '─',
                    _ => ' ',
                }
            } else {
                ' '
            };
            prefix.push(ch);
        }
        prefix
    }
}

impl Component for TreeList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme = theme();
        let muted = |text: &str| truncate_to_width(&theme.fg(ThemeColor::Muted, text), width);
        if self.filtered.is_empty() {
            return vec![muted("  No entries found"), muted(&format!("  (0/0){}", self.status_labels()))];
        }

        let (start, end) = visible_window(self.selected_index, self.filtered.len(), self.max_visible_lines);
        let mut rows = Vec::with_capacity(end - start);
        for index in start..end {
            let flat = self.filtered[index];
            let row = &self.flat_nodes[flat];
            let node = self.node_of(flat);
            let id = node.entry.id();
            let is_selected = index == self.selected_index;
            let cursor = if is_selected { theme.fg(ThemeColor::Accent, "› ") } else { "  ".to_string() };
            let shows_fold_in_connector = row.show_connector && !row.is_virtual_root_child;
            let fold_marker = if self.folded.contains(id) && !shows_fold_in_connector {
                theme.fg(ThemeColor::Accent, "⊞ ")
            } else {
                String::new()
            };
            let path_marker =
                if self.active_path.contains(id) { theme.fg(ThemeColor::Accent, "• ") } else { String::new() };
            let label = match node.label.as_deref() {
                Some(label) if !label.is_empty() => theme.fg(ThemeColor::Warning, &format!("[{label}] ")),
                _ => String::new(),
            };
            let label_time = match (&node.label, &node.label_timestamp) {
                (Some(label), Some(stamp)) if self.show_label_timestamps && !label.is_empty() && !stamp.is_empty() => {
                    theme.fg(ThemeColor::Muted, &format!("{} ", format_label_timestamp(stamp)))
                }
                _ => String::new(),
            };
            let content = entry_display_text(node, &self.tool_calls, is_selected);
            let prefix_part = theme.fg(ThemeColor::Dim, &self.row_prefix(flat)) + &fold_marker + &path_marker;
            let anchor_col = visible_width(&prefix_part);
            let mut gutter = cursor;
            let mut body = format!("{prefix_part}{label}{label_time}{content}");
            if is_selected {
                gutter = theme.bg(ThemeBg::SelectedBg, &gutter);
                body = theme.bg(ThemeBg::SelectedBg, &body);
            }
            let body_width = visible_width(&body);
            rows.push(ViewportRow { gutter, body, anchor_col, body_width, is_selected });
        }

        let mut lines = render_horizontal_viewport(&rows, width);
        let position = format!("  ({}/{}){}", self.selected_index + 1, self.filtered.len(), self.status_labels());
        lines.push(muted(&position));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        self.handle_key(data);
    }

    fn handles_input(&self) -> bool {
        true
    }
}
