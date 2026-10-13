//! Small builders shared by the dialog components.

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::components::{Spacer, Text, TruncatedText};
use pi_tui::{ComponentRef, component};

use super::DynamicBorder;
use crate::theme::{ThemeColor, theme};

/// A [`Text`] child.
pub fn text(content: &str, padding_x: usize, padding_y: usize) -> ComponentRef {
    component(Text::new(content, padding_x, padding_y))
}

/// A [`TruncatedText`] child.
pub fn truncated(content: &str, padding_x: usize, padding_y: usize) -> ComponentRef {
    component(TruncatedText::new(content, padding_x, padding_y))
}

/// A [`Spacer`] child.
pub fn spacer(lines: usize) -> ComponentRef {
    component(Spacer::new(lines))
}

/// A [`DynamicBorder`] child in the default color.
pub fn border() -> ComponentRef {
    component(DynamicBorder::new())
}

/// Text in a theme color.
pub fn fg(color: ThemeColor, content: &str) -> String {
    theme().fg(color, content)
}

/// Bold text.
pub fn bold(content: &str) -> String {
    theme().bold(content)
}

/// A callback shared between a component and the lists it rebuilds.
pub type Shared<F> = Rc<RefCell<Box<F>>>;

/// Wraps a callback for sharing.
pub fn shared<F: ?Sized>(callback: Box<F>) -> Shared<F> {
    Rc::new(RefCell::new(callback))
}

/// Upstream `Math.max(0, Math.min(selected - floor(max / 2), len - max))` and the end index.
pub fn visible_window(selected: usize, len: usize, max_visible: usize) -> (usize, usize) {
    let start = (selected as i64 - (max_visible / 2) as i64).min(len as i64 - max_visible as i64).max(0) as usize;
    (start, (start + max_visible).min(len))
}

/// [`pi_tui::fuzzy::fuzzy_filter`] with computed search text.
pub fn fuzzy_filter_by<T: Clone>(items: &[T], query: &str, get_text: impl Fn(&T) -> String) -> Vec<T> {
    let pairs: Vec<(T, String)> = items.iter().map(|item| (item.clone(), get_text(item))).collect();
    pi_tui::fuzzy::fuzzy_filter(pairs, query, |(_, text): &(T, String)| text.as_str())
        .into_iter()
        .map(|(item, _)| item)
        .collect()
}

/// Approximates JavaScript `localeCompare` for identifiers: case-insensitive first, then
/// lowercase before uppercase.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

/// Upstream `frame` of the `/llama` and `/mcp` views: accent borders, a bold title, the body,
/// and a dim footer line.
pub fn frame(title: &str, body: Vec<pi_tui::ComponentRef>, footer: Option<&str>) -> pi_tui::Container {
    let accent_border = || component(DynamicBorder::with_color(|text| theme().fg(ThemeColor::Accent, text)));
    let mut container = pi_tui::Container::new();
    container.add_child(accent_border());
    container.add_child(self::text(&fg(ThemeColor::Accent, &bold(title)), 1, 0));
    for child in body {
        container.add_child(child);
    }
    if let Some(footer) = footer.filter(|footer| !footer.is_empty()) {
        container.add_child(spacer(1));
        container.add_child(self::text(&fg(ThemeColor::Dim, footer), 1, 0));
    }
    container.add_child(accent_border());
    container
}

/// Cuts lines wider than `width`, as the `/llama` and `/mcp` views do before returning them.
pub fn clamp_lines(lines: Vec<String>, width: usize) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            if pi_tui::utils::visible_width(&line) > width {
                pi_tui::utils::truncate_to_width_with(&line, width, "", false)
            } else {
                line
            }
        })
        .collect()
}
