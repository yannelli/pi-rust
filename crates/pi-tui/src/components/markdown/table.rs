//! Table rendering with width-aware column sizing and cell wrapping.

use super::Markdown;
use super::render::StyleContext;
use super::token::{TableCell, TableToken};
use crate::utils::{is_js_whitespace, visible_width, wrap_text_with_ansi};

const MAX_UNBROKEN_WORD_WIDTH: usize = 30;

/// Visible width of the longest whitespace-separated word, capped at `max_width`.
fn longest_word_width(text: &str, max_width: usize) -> usize {
    text.split(is_js_whitespace).filter(|word| !word.is_empty()).map(visible_width).max().unwrap_or(0).min(max_width)
}

/// Wraps a cell, resetting text styles after each non-final fragment and restoring the
/// surrounding style before padding and borders.
fn wrap_cell_text(text: &str, max_width: usize, style_prefix: &str) -> Vec<String> {
    let lines = wrap_text_with_ansi(text, max_width.max(1));
    let count = lines.len();
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let reset = if index + 1 < count { "\x1b[22;23;24;25;27;28;29;39m" } else { "" };
            format!("{line}{reset}{style_prefix}")
        })
        .collect()
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(visible_width(text))))
}

/// JavaScript `Math.floor((part / total) * amount)`, keeping its floating-point rounding.
fn floor_share(part: usize, total: usize, amount: usize) -> usize {
    ((part as f64 / total as f64) * amount as f64).floor() as usize
}

/// Column widths that fit `available` cells, growing columns in proportion to their need.
fn column_widths(natural: &[usize], min_words: &[usize], available_for_cells: usize, available: usize) -> Vec<usize> {
    let columns = natural.len();
    let border_overhead = 3 * columns + 1;
    let mut min_widths = min_words.to_vec();
    let mut min_total: usize = min_widths.iter().sum();
    if min_total > available_for_cells {
        min_widths = vec![1; columns];
        let remaining = available_for_cells.saturating_sub(columns);
        if remaining > 0 {
            let total_weight: usize = min_words.iter().map(|w| w.saturating_sub(1)).sum();
            let growth: Vec<usize> = min_words
                .iter()
                .map(|w| if total_weight > 0 { floor_share(w.saturating_sub(1), total_weight, remaining) } else { 0 })
                .collect();
            for (width, grow) in min_widths.iter_mut().zip(&growth) {
                *width += grow;
            }
            let mut leftover = remaining - growth.iter().sum::<usize>();
            for width in min_widths.iter_mut() {
                if leftover == 0 {
                    break;
                }
                *width += 1;
                leftover -= 1;
            }
        }
        min_total = min_widths.iter().sum();
    }

    if natural.iter().sum::<usize>() + border_overhead <= available {
        return natural.iter().zip(&min_widths).map(|(n, m)| *n.max(m)).collect();
    }
    let grow_potential: usize = natural.iter().zip(&min_widths).map(|(n, m)| n.saturating_sub(*m)).sum();
    let extra = available_for_cells.saturating_sub(min_total);
    let mut widths: Vec<usize> = min_widths
        .iter()
        .zip(natural)
        .map(|(m, n)| {
            let delta = n.saturating_sub(*m);
            m + if grow_potential > 0 { floor_share(delta, grow_potential, extra) } else { 0 }
        })
        .collect();
    let mut remaining = available_for_cells as i64 - widths.iter().sum::<usize>() as i64;
    while remaining > 0 {
        let mut grew = false;
        for (width, natural) in widths.iter_mut().zip(natural) {
            if remaining > 0 && *width < *natural {
                *width += 1;
                remaining -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    widths
}

impl Markdown {
    fn render_cell(&mut self, cell: &TableCell, context: Option<&StyleContext>) -> String {
        self.render_inline_tokens(&cell.tokens.borrow(), context)
    }

    pub(super) fn render_table(
        &mut self,
        table: &TableToken,
        available_width: usize,
        next: Option<&str>,
        context: Option<&StyleContext>,
    ) -> Vec<String> {
        let columns = table.header.len();
        if columns == 0 {
            return Vec::new();
        }
        let border_overhead = 3 * columns + 1;
        if available_width < border_overhead + columns {
            // Too narrow for a stable table: fall back to the raw Markdown.
            let mut lines =
                if table.raw.is_empty() { Vec::new() } else { wrap_text_with_ansi(&table.raw, available_width) };
            if next.is_some_and(|next| next != "space") {
                lines.push(String::new());
            }
            return lines;
        }
        let available_for_cells = available_width - border_overhead;

        let mut natural = vec![0; columns];
        let mut min_words = vec![1; columns];
        for (index, cell) in table.header.iter().enumerate() {
            let text = self.render_cell(cell, context);
            natural[index] = visible_width(&text);
            min_words[index] = longest_word_width(&text, MAX_UNBROKEN_WORD_WIDTH).max(1);
        }
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate().take(columns) {
                let text = self.render_cell(cell, context);
                natural[index] = natural[index].max(visible_width(&text));
                min_words[index] = min_words[index].max(longest_word_width(&text, MAX_UNBROKEN_WORD_WIDTH));
            }
        }
        let widths = column_widths(&natural, &min_words, available_for_cells, available_width);
        let style_prefix = context.map_or("", |context| context.style_prefix.as_str()).to_string();
        let border = |left: &str, middle: &str, right: &str| {
            let cells: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            format!("{left}─{}─{right}", cells.join(&format!("─{middle}─")))
        };

        let mut lines = vec![border("┌", "┬", "┐")];
        let header: Vec<Vec<String>> = table
            .header
            .iter()
            .enumerate()
            .map(|(index, cell)| wrap_cell_text(&self.render_cell(cell, context), widths[index], &style_prefix))
            .collect();
        let header_height = header.iter().map(Vec::len).max().unwrap_or(0);
        for line_index in 0..header_height {
            let parts: Vec<String> = header
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    (self.theme.bold)(&pad(cell.get(line_index).map_or("", String::as_str), widths[column]))
                })
                .collect();
            lines.push(format!("│ {} │", parts.join(" │ ")));
        }
        let separator = border("├", "┼", "┤");
        lines.push(separator.clone());

        for (row_index, row) in table.rows.iter().enumerate() {
            let cells: Vec<Vec<String>> = row
                .iter()
                .enumerate()
                .take(columns)
                .map(|(index, cell)| wrap_cell_text(&self.render_cell(cell, context), widths[index], &style_prefix))
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(0);
            for line_index in 0..height {
                let parts: Vec<String> = cells
                    .iter()
                    .enumerate()
                    .map(|(column, cell)| pad(cell.get(line_index).map_or("", String::as_str), widths[column]))
                    .collect();
                lines.push(format!("│ {} │", parts.join(" │ ")));
            }
            if row_index + 1 < table.rows.len() {
                lines.push(separator.clone());
            }
        }
        lines.push(border("└", "┴", "┘"));
        if next.is_some_and(|next| next != "space") {
            lines.push(String::new());
        }
        lines
    }
}
